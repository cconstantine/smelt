//! Script MCP calls through the agent (SME-359, T20–T26): the agent's
//! real router on loopback with a tungstenite client playing smelt, its
//! Unix socket in a directory of the test's own, and raw HTTP on that
//! socket playing the script.

use super::*;
use crate::agent_protocol::McpRequest;
use crate::cli;
use crate::mcp::{MAX_WAITING, bind_mcp_socket, serve_mcp};

/// A directory of this test's own for the socket. Unix socket paths are
/// short (108 bytes), so it's under `/tmp`, not the target directory.
fn socket_dir(test: &str) -> std::path::PathBuf {
    let dir = std::path::PathBuf::from(format!("/tmp/smelt-mcp-test-{}-{test}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("socket dir");
    dir
}

/// The agent with its MCP socket served at `<dir>/mcp.sock`.
async fn serve_agent_with_socket(test: &str) -> (std::net::SocketAddr, String, Arc<AppState>) {
    let state = new_state();
    let path = socket_dir(test).join("mcp.sock").to_string_lossy().into_owned();
    let listener = bind_mcp_socket(&state, &path).expect("bind the MCP socket");
    tokio::spawn(serve_mcp(listener, state.clone()));
    let addr = serve_agent_with(state.clone()).await;
    (addr, path, state)
}

/// One HTTP request on the socket, as a script would send it; the
/// answer's status and body.
async fn script(path: &str, request: String) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::UnixStream::connect(path).await.expect("connect to the MCP socket");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut answer = Vec::new();
    tokio::time::timeout(Duration::from_secs(200), stream.read_to_end(&mut answer))
        .await
        .expect("an answer")
        .expect("read");
    cli::parse_response(&answer).expect("an HTTP answer")
}

fn call_request(body: &str) -> String {
    format!(
        "POST /v1/call HTTP/1.1\r\nHost: smelt\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn list_request(query: &str) -> String {
    format!("GET /v1/tools{query} HTTP/1.1\r\nHost: smelt\r\nConnection: close\r\n\r\n")
}

/// The next `mcp_request` smelt's end gets, skipping output lines.
async fn next_mcp_request(client: &mut Client) -> (u64, Option<String>, McpRequest) {
    loop {
        match next(client).await {
            AgentMessage::McpRequest { call_id, command_id, request } => return (call_id, command_id, request),
            AgentMessage::Output { .. } => continue,
            other => panic!("expected an mcp_request, got {other:?}"),
        }
    }
}

async fn answer(client: &mut Client, call_id: u64, status: u16, body: &str) {
    send(client, &ClientMessage::McpResponse { call_id, status, body: body.into() }).await;
}

/// T20: the body goes up as the script wrote it, and the status and body
/// smelt answers with come back as they are.
#[tokio::test]
async fn test_a_script_call_goes_up_the_websocket_and_its_answer_comes_back() {
    let (addr, path, _state) = serve_agent_with_socket("happy").await;
    let mut smelt = connect_ready(addr).await;
    let body = r#"{"tool":"linear/list_issues", "arguments": {"query":"x  y"}}"#;
    let call = tokio::spawn({
        let path = path.clone();
        async move { script(&path, call_request(body)).await }
    });
    let (call_id, command_id, request) = next_mcp_request(&mut smelt).await;
    assert_eq!(request, McpRequest::Call { body: body.into() }, "the body, unchanged");
    assert_eq!(command_id, None, "the test process is in no terminal");
    answer(&mut smelt, call_id, 201, r#"{"content":[],"text":"made"}"#).await;
    assert_eq!(call.await.expect("join"), (201, r#"{"content":[],"text":"made"}"#.to_string()));

    let list = tokio::spawn(async move { script(&path, list_request("?server=linear.app")).await });
    let (call_id, _, request) = next_mcp_request(&mut smelt).await;
    assert_eq!(request, McpRequest::List { server: Some("linear.app".into()) });
    answer(&mut smelt, call_id, 200, r#"{"tools":[]}"#).await;
    assert_eq!(list.await.expect("join"), (200, r#"{"tools":[]}"#.to_string()));
}

/// T21: with no connection from smelt, the script hears so at once, and
/// that nothing was sent.
#[tokio::test]
async fn test_a_call_with_smelt_not_connected_fails_at_once() {
    let (_addr, path, _state) = serve_agent_with_socket("unconnected").await;
    let started = std::time::Instant::now();
    let (status, body) = script(&path, call_request(r#"{"tool":"a/b"}"#)).await;
    assert_eq!(status, 503, "{body}");
    assert!(started.elapsed() < Duration::from_millis(500), "took {:?}", started.elapsed());
    assert!(body.contains(r#""outcome":"not_sent""#), "{body}");
}

/// T22: the connection closing while a call waits ends the call with an
/// unknown outcome, at once.
#[tokio::test]
async fn test_a_call_waiting_when_the_connection_closes_has_an_unknown_outcome() {
    let (addr, path, _state) = serve_agent_with_socket("closes").await;
    let mut smelt = connect_ready(addr).await;
    let call = tokio::spawn(async move { script(&path, call_request(r#"{"tool":"a/b"}"#)).await });
    let _ = next_mcp_request(&mut smelt).await;
    drop(smelt);
    let (status, body) = tokio::time::timeout(WAIT, call).await.expect("answered at once").expect("join");
    assert_eq!(status, 503, "{body}");
    assert!(body.contains(r#""outcome":"unknown""#), "{body}");
}

/// T23: a newer connection taking over fails the call waiting on the
/// older one, and the old call's id answered on the new connection
/// reaches nothing; the next call goes up the new one.
#[tokio::test]
async fn test_a_newer_connection_fails_the_calls_waiting_on_the_old_one() {
    let (addr, path, _state) = serve_agent_with_socket("takeover").await;
    let mut old = connect_ready(addr).await;
    let call = tokio::spawn({
        let path = path.clone();
        async move { script(&path, call_request(r#"{"tool":"a/b"}"#)).await }
    });
    let (old_call, _, _) = next_mcp_request(&mut old).await;
    let mut new = connect_ready(addr).await;
    let (status, body) = tokio::time::timeout(WAIT, call).await.expect("answered at once").expect("join");
    assert_eq!(status, 503, "{body}");
    assert!(body.contains(r#""outcome":"unknown""#), "{body}");

    answer(&mut new, old_call, 200, r#"{"late":true}"#).await;
    let next_call = tokio::spawn(async move { script(&path, call_request(r#"{"tool":"a/c"}"#)).await });
    let (call_id, _, _) = next_mcp_request(&mut new).await;
    assert_ne!(call_id, old_call, "a call id is never reused");
    answer(&mut new, call_id, 200, r#"{"mine":true}"#).await;
    assert_eq!(next_call.await.expect("join"), (200, r#"{"mine":true}"#.to_string()));
}

/// T24: an answer for an id nobody waits for is dropped, and the next
/// call still gets its own.
#[tokio::test]
async fn test_an_answer_for_an_unknown_call_is_dropped() {
    let (addr, path, _state) = serve_agent_with_socket("unknown-id").await;
    let mut smelt = connect_ready(addr).await;
    answer(&mut smelt, 9_999, 200, r#"{"stray":true}"#).await;
    let call = tokio::spawn(async move { script(&path, call_request(r#"{"tool":"a/b"}"#)).await });
    let (call_id, _, _) = next_mcp_request(&mut smelt).await;
    answer(&mut smelt, call_id + 1, 200, r#"{"wrong":true}"#).await;
    answer(&mut smelt, call_id, 200, r#"{"right":true}"#).await;
    assert_eq!(call.await.expect("join"), (200, r#"{"right":true}"#.to_string()));
}

/// The agent holds at most `MAX_WAITING` calls; one more is refused at
/// once, and doesn't go up.
#[tokio::test]
async fn test_a_call_past_the_agents_waiting_bound_is_refused() {
    let (addr, path, _state) = serve_agent_with_socket("bound").await;
    let mut smelt = connect_ready(addr).await;
    let mut waiting = Vec::new();
    for _ in 0..MAX_WAITING {
        let path = path.clone();
        waiting.push(tokio::spawn(async move { script(&path, call_request(r#"{"tool":"a/b"}"#)).await }));
        let _ = next_mcp_request(&mut smelt).await;
    }
    let (status, body) = script(&path, call_request(r#"{"tool":"a/b"}"#)).await;
    assert_eq!(status, 429, "{body}");
    drop(smelt);
    for call in waiting {
        assert_eq!(call.await.expect("join").0, 503);
    }
}

/// T25: a call from a terminal's command is that command's, and a
/// process double-forked away from its shell is no command's. The
/// script's own shape: `curl` on `$SMELT_MCP_SOCKET`, as `smelt-mcp`
/// and a Python script would reach it.
#[tokio::test]
async fn test_a_call_from_a_terminal_command_names_that_command() {
    let (addr, path, _state) = serve_agent_with_socket("attribution").await;
    let mut smelt = connect_ready(addr).await;
    create_terminal(&mut smelt, "t1").await;
    let curl = r#"curl -s --unix-socket "$SMELT_MCP_SOCKET" -d '{"tool":"a/b"}' http://smelt/v1/call"#;
    run(&mut smelt, "t1", "c1", &format!("echo \"socket=$SMELT_MCP_SOCKET\"; {curl}")).await;
    let mut socket_line = None;
    let call_id = loop {
        match next(&mut smelt).await {
            AgentMessage::Output { data, .. } => socket_line = Some(data),
            AgentMessage::McpRequest { call_id, command_id, .. } => {
                assert_eq!(command_id.as_deref(), Some("c1"));
                break call_id;
            }
            other => panic!("unexpected {other:?}"),
        }
    };
    assert_eq!(socket_line, Some(format!("socket={path}")), "the shell has the socket's path");
    answer(&mut smelt, call_id, 200, r#"{"ok":1}"#).await;
    loop {
        match next(&mut smelt).await {
            AgentMessage::Exit { id, code, .. } => {
                assert_eq!((id.as_str(), code), ("c1", 0));
                break;
            }
            AgentMessage::Output { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }

    // Double-forked: its parent exits, and it's reparented away from the
    // shell's tree. It waits a moment so the reparenting has happened.
    run(&mut smelt, "t1", "c2", &format!("(setsid sh -c 'sleep 0.3; {}' >/dev/null 2>&1 &)", curl.replace('\'', "'\\''"))).await;
    let (call_id, command_id, _) = next_mcp_request_after_exit(&mut smelt, "c2").await;
    assert_eq!(command_id, None, "a double-forked process belongs to no command");
    answer(&mut smelt, call_id, 200, "{}").await;
}

/// The next `mcp_request`, letting the command `id` finish first.
async fn next_mcp_request_after_exit(client: &mut Client, id: &str) -> (u64, Option<String>, McpRequest) {
    loop {
        match next(client).await {
            AgentMessage::McpRequest { call_id, command_id, request } => return (call_id, command_id, request),
            AgentMessage::Exit { id: exited, .. } => assert_eq!(exited, id),
            AgentMessage::Output { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// T26: the socket is the agent user's alone, and a stale one an earlier
/// agent left at the path is replaced.
#[tokio::test]
async fn test_the_socket_is_private_and_replaces_a_stale_one() {
    use std::os::unix::fs::PermissionsExt;
    let path = socket_dir("stale").join("mcp.sock");
    // A socket file nobody serves, as an agent that died leaves it.
    drop(std::os::unix::net::UnixListener::bind(&path).expect("a stale socket"));
    assert!(path.exists());
    let state = new_state();
    let listener = bind_mcp_socket(&state, &path.to_string_lossy()).expect("binds over the stale socket");
    let mode = std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    drop(listener);
}

/// `smelt-mcp`: the request it makes, its exit codes and its output.
#[tokio::test]
async fn test_the_cli_calls_the_socket_and_exits_by_the_answer() {
    let (addr, path, _state) = serve_agent_with_socket("cli").await;
    let mut smelt = connect_ready(addr).await;
    for (status, code) in [(200, 0), (422, 1), (403, 2), (429, 2), (404, 3), (502, 3)] {
        let args = vec!["call".to_string(), "linear/save_comment".to_string(), "-".to_string()];
        let cli = tokio::task::spawn_blocking({
            let path = path.clone();
            move || cli::run(&args, Some(&path), &mut std::io::Cursor::new(b" {\"body\":\"hi\"}\n".to_vec()))
        });
        let (call_id, _, request) = next_mcp_request(&mut smelt).await;
        assert_eq!(request, McpRequest::Call { body: r#"{"tool":"linear/save_comment","arguments":{"body":"hi"}}"#.into() });
        answer(&mut smelt, call_id, status, r#"{"x":1}"#).await;
        assert_eq!(cli.await.expect("join"), (code, r#"{"x":1}"#.to_string(), String::new()), "status {status}");
    }
    let cli = tokio::task::spawn_blocking({
        let path = path.clone();
        move || cli::run(&["tools".to_string(), "a b".to_string()], Some(&path), &mut std::io::empty())
    });
    let (call_id, _, request) = next_mcp_request(&mut smelt).await;
    assert_eq!(request, McpRequest::List { server: Some("a b".into()) }, "the server name, decoded again");
    answer(&mut smelt, call_id, 200, "{}").await;
    assert_eq!(cli.await.expect("join").0, 0);

    let (code, out, err) = cli::run(&["tools".to_string()], None, &mut std::io::empty());
    assert_eq!((code, out.as_str()), (3, ""));
    assert!(err.contains("SMELT_MCP_SOCKET isn't set"), "{err}");
    let (code, _, err) = cli::run(&["frobnicate".to_string()], Some(&path), &mut std::io::empty());
    assert_eq!(code, 3);
    assert!(err.starts_with("usage:"), "{err}");
}

#[test]
fn test_a_chunked_answer_is_read_whole() {
    let answer = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n";
    assert_eq!(cli::parse_response(answer), Some((200, r#"{"a":1}"#.to_string())));
}

/// Review round 1, L6: an answer as large as smelt sends (its 8 MiB body
/// cap, escaped inside the message) arrives whole. axum's default 16 MiB
/// frame limit closed the connection on it.
#[tokio::test]
async fn test_a_large_answer_reaches_the_script_whole() {
    let (addr, path, _state) = serve_agent_with_socket("large").await;
    let mut smelt = connect_ready(addr).await;
    let call = tokio::spawn(async move { script(&path, call_request(r#"{"tool":"a/b"}"#)).await });
    let (call_id, _, _) = next_mcp_request(&mut smelt).await;
    // 9 MiB of quotes: about 18 MiB once escaped into the message.
    let body = format!("{{\"text\":\"{}\"}}", "\\\"".repeat(9 << 19));
    answer(&mut smelt, call_id, 200, &body).await;
    let (status, got) = tokio::time::timeout(WAIT * 4, call).await.expect("answered").expect("join");
    assert_eq!(status, 200, "{}", &got[..got.len().min(200)]);
    assert_eq!(got.len(), body.len());
}

/// A [`Shell`] record as `start_shell` makes one, for `bash_pid`, with
/// `stdin` from `child` (any process: nothing writes to it here).
fn shell_record(child: &mut tokio::process::Child, bash_pid: i32, current: &str, ended: bool) -> Arc<Shell> {
    Arc::new(Shell {
        stdin: AsyncMutex::new(child.stdin.take().expect("stdin piped")),
        bash_pid: bash_pid as u32,
        current: AsyncMutex::new(Some(current.to_string())),
        marker: Arc::new(std::sync::Mutex::new(None)),
        ended: std::sync::atomic::AtomicBool::new(ended),
    })
}

/// SME-386 M6: an ended shell's pid is free, and the kernel can hand it
/// to a process in a live terminal's tree. The walk skips ended shells,
/// so the call is the live terminal's command, not the old one's.
#[tokio::test]
async fn test_a_reused_pid_of_an_ended_shell_names_the_live_command() {
    use tokio::io::AsyncBufReadExt;
    // The live terminal's shell, and a child of it.
    let mut live = Command::new("sh")
        .args(["-c", "sleep 30 & echo $!; wait"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn the live shell");
    let live_pid = live.id().expect("running") as i32;
    let mut line = String::new();
    let stdout = live.stdout.take().expect("stdout piped");
    tokio::time::timeout(WAIT, tokio::io::BufReader::new(stdout).read_line(&mut line))
        .await
        .expect("the child's pid in time")
        .expect("read");
    let child: i32 = line.trim().parse().expect("a pid");
    // The ended shell, its pid now the live shell's child's.
    let mut stand_in = Command::new("cat").stdin(Stdio::piped()).kill_on_drop(true).spawn().expect("spawn");
    let state = new_state();
    state.terminals.lock().await.insert("t-old".into(), shell_record(&mut stand_in, child, "c-old", true));
    state.terminals.lock().await.insert("t-live".into(), shell_record(&mut live, live_pid, "c-live", false));
    let named = command_for_pid(&state, child).await;
    let _ = signal::kill(Pid::from_raw(child), Signal::SIGKILL);
    assert_eq!(named.as_deref(), Some("c-live"));
}
