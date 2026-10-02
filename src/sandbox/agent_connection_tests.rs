use super::*;
use crate::agent_protocol::{DirEntry, Reply};
use std::sync::atomic::{AtomicUsize, Ordering};

/// What the fake agent sends first on every connection.
#[derive(Clone)]
enum Greeting {
    Hello(ProtocolVersion),
    /// A raw text frame, e.g. a protocol-0 output line.
    Raw(String),
    Nothing,
}

enum Out {
    Message(AgentMessage),
    Raw(String),
    Close,
}

/// One connection the fake agent accepted: what smelt sent on it, and a
/// way to answer.
struct FakeConnection {
    received: mpsc::UnboundedReceiver<ClientMessage>,
    send: mpsc::UnboundedSender<Out>,
    /// Fires when the socket ends, whoever ended it.
    ended: tokio::sync::oneshot::Receiver<()>,
}

impl FakeConnection {
    async fn next_request(&mut self) -> ClientMessage {
        tokio::time::timeout(Duration::from_secs(5), self.received.recv())
            .await
            .expect("smelt sent nothing within 5s")
            .expect("the connection ended")
    }

    fn reply(&self, request_id: u64, result: Reply) {
        let _ = self.send.send(Out::Message(AgentMessage::Reply { request_id, result }));
    }
}

struct FakeAgent {
    connections: mpsc::UnboundedReceiver<FakeConnection>,
    dials: Arc<AtomicUsize>,
}

impl FakeAgent {
    async fn next_connection(&mut self) -> FakeConnection {
        tokio::time::timeout(Duration::from_secs(5), self.connections.recv())
            .await
            .expect("smelt didn't connect within 5s")
            .expect("the fake agent stopped")
    }
}

/// Dials the fake agent's port, after `delay`, counting dials.
struct FakeDialer {
    addr: std::net::SocketAddr,
    delay: Duration,
    dials: Arc<AtomicUsize>,
}

impl AgentDialer for FakeDialer {
    fn dial(&self, _pod_id: i64) -> BoxFuture<'_, Result<Box<dyn AgentIo>, SandboxError>> {
        Box::pin(async move {
            self.dials.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            let stream = tokio::net::TcpStream::connect(self.addr).await.map_err(SandboxError::Io)?;
            Ok(Box::new(stream) as Box<dyn AgentIo>)
        })
    }

    fn is_running(&self, _pod_id: i64) -> BoxFuture<'_, Result<bool, SandboxError>> {
        Box::pin(async { Ok(true) })
    }

    fn death_reason(&self, _pod_id: i64) -> BoxFuture<'_, Option<Option<String>>> {
        Box::pin(async { None })
    }
}

/// A fake agent for `pod_id`, reached through `dialer_for`.
async fn fake_agent(pod_id: i64, greeting: Greeting, dial_delay: Duration) -> FakeAgent {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (connections_tx, connections) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else { continue };
            let (received_tx, received) = mpsc::unbounded_channel();
            let (send, mut outgoing) = mpsc::unbounded_channel::<Out>();
            let (ended_tx, ended) = tokio::sync::oneshot::channel();
            let _ = connections_tx.send(FakeConnection { received, send, ended });
            let greeting = greeting.clone();
            tokio::spawn(async move {
                let first = match greeting {
                    Greeting::Hello(version) => Some(serde_json::to_string(&AgentMessage::Hello(version)).expect("json")),
                    Greeting::Raw(text) => Some(text),
                    Greeting::Nothing => None,
                };
                if let Some(first) = first {
                    let _ = ws.send(WsMessage::Text(first.into())).await;
                }
                loop {
                    tokio::select! {
                        frame = ws.next() => match frame {
                            Some(Ok(WsMessage::Text(text))) => {
                                let message = serde_json::from_str(&text).expect("smelt sent a v1 message");
                                let _ = received_tx.send(message);
                            }
                            Some(Ok(_)) => {}
                            _ => break,
                        },
                        out = outgoing.recv() => match out {
                            Some(Out::Message(message)) => {
                                let text = serde_json::to_string(&message).expect("json");
                                let _ = ws.send(WsMessage::Text(text.into())).await;
                            }
                            Some(Out::Raw(text)) => {
                                let _ = ws.send(WsMessage::Text(text.into())).await;
                            }
                            Some(Out::Close) | None => {
                                let _ = ws.close(None).await;
                                break;
                            }
                        },
                    }
                }
                let _ = ended_tx.send(());
            });
        }
    });
    let dials = Arc::new(AtomicUsize::new(0));
    let dialer = FakeDialer { addr, delay: dial_delay, dials: dials.clone() };
    test_dialers().lock().unwrap_or_else(|e| e.into_inner()).insert(pod_id, Arc::new(dialer));
    FakeAgent { connections, dials }
}

/// A conversation and a live pod row for it, with a pod id no other
/// test uses (the registry and the fake dialers are keyed by pod id).
async fn pod_row(pool: &PgPool) -> (i64, i64) {
    static NEXT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let unique = (std::process::id() as i64 % 10_000) * 1_000_000
        + NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        + 5_000_000_000;
    sqlx::query("SELECT setval(pg_get_serial_sequence('sandbox_pods', 'id'), $1)")
        .bind(unique)
        .execute(pool)
        .await
        .expect("move the pod id sequence");
    let conversation = db::create_conversation(pool).await.expect("conversation");
    let pod = db::create_sandbox_pod(pool, conversation.id).await.expect("pod row");
    (conversation.id, pod.id)
}

fn current() -> Greeting {
    Greeting::Hello(PROTOCOL_VERSION)
}

#[sqlx::test]
async fn test_replies_resolve_their_own_requests_in_any_order(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let mut agent = fake_agent(pod_id, current(), Duration::ZERO).await;
    let conn = reconnect_if_needed(&pool, pod_id).await.expect("connects");
    let mut fake = agent.next_connection().await;

    // Two requests about the same terminal, the old correlation key.
    let first = conn.request(|request_id| ClientMessage::CreateTerminal { request_id, terminal_id: "7".into() });
    let second = conn.request(|request_id| ClientMessage::ListDirectory { request_id, path: "/b".into() });
    let answer = async {
        let a = fake.next_request().await;
        let b = fake.next_request().await;
        let (ClientMessage::CreateTerminal { request_id: a, .. }, ClientMessage::ListDirectory { request_id: b, .. }) = (a, b) else {
            panic!("unexpected requests");
        };
        // Answered in the opposite order.
        fake.reply(b, Reply::DirectoryListed { entries: vec![DirEntry { name: "b".into(), is_dir: false, size: Some(1) }] });
        fake.reply(a, Reply::Error { message: "terminal_id already exists".into() });
    };
    let (first, second, ()) = tokio::join!(first, second, answer);
    assert_eq!(first, Err(AgentRequestError::Rejected("terminal_id already exists".into())));
    assert!(matches!(second, Ok(Reply::DirectoryListed { entries }) if entries[0].name == "b"));
}

/// The agent's own reason reaches the model, for a terminal action too,
/// instead of "no such terminal".
#[sqlx::test]
async fn test_an_agent_refusal_of_create_terminal_reaches_the_model_verbatim(pool: PgPool) {
    let (conversation_id, pod_id) = pod_row(&pool).await;
    let mut agent = fake_agent(pod_id, current(), Duration::ZERO).await;
    let created = tokio::spawn({
        let pool = pool.clone();
        async move { create_terminal(&pool, conversation_id).await }
    });
    let mut fake = agent.next_connection().await;
    let ClientMessage::CreateTerminal { request_id, .. } = fake.next_request().await else {
        panic!("expected create_terminal");
    };
    fake.reply(request_id, Reply::Error { message: "failed to spawn shell: out of memory".into() });
    let err = created.await.expect("join").expect_err("the agent refused");
    assert_eq!(err.to_string(), "failed to spawn shell: out of memory");
    assert!(
        db::list_sandbox_terminals_for_pod(&pool, pod_id).await.expect("list").is_empty(),
        "the refused terminal's row is still live"
    );
}

/// A reply that comes after its request timed out answers nothing, and
/// the next request still gets its own.
#[sqlx::test]
async fn test_a_request_times_out_and_its_late_reply_answers_nothing(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let mut agent = fake_agent(pod_id, current(), Duration::ZERO).await;
    let conn = reconnect_if_needed(&pool, pod_id).await.expect("connects");
    let mut fake = agent.next_connection().await;

    let timed_out = conn
        .request_within(Duration::from_millis(200), |request_id| ClientMessage::ListDirectory { request_id, path: "/slow".into() })
        .await;
    assert_eq!(timed_out, Err(AgentRequestError::Timeout));
    let ClientMessage::ListDirectory { request_id: slow, .. } = fake.next_request().await else { panic!() };

    let next = conn.request(|request_id| ClientMessage::ListDirectory { request_id, path: "/fast".into() });
    let answer = async {
        let ClientMessage::ListDirectory { request_id: fast, .. } = fake.next_request().await else { panic!() };
        fake.reply(slow, Reply::DirectoryListed { entries: vec![] });
        fake.reply(fast, Reply::DirectoryListed { entries: vec![DirEntry { name: "fast".into(), is_dir: true, size: None }] });
    };
    let (next, ()) = tokio::join!(next, answer);
    assert!(matches!(next, Ok(Reply::DirectoryListed { entries }) if entries.len() == 1));
}

/// Waiters learn at once that the connection dropped, instead of
/// sitting out the whole timeout.
#[sqlx::test]
async fn test_a_dropped_connection_fails_waiting_requests_at_once(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let mut agent = fake_agent(pod_id, current(), Duration::ZERO).await;
    let conn = reconnect_if_needed(&pool, pod_id).await.expect("connects");
    let mut fake = agent.next_connection().await;

    let started = tokio::time::Instant::now();
    let waiting = conn.request(|request_id| ClientMessage::ListDirectory { request_id, path: "/".into() });
    let drop_it = async {
        fake.next_request().await;
        let _ = fake.send.send(Out::Close);
    };
    let (result, ()) = tokio::join!(waiting, drop_it);
    assert_eq!(result, Err(AgentRequestError::Disconnected));
    assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
    test_dialers().lock().unwrap_or_else(|e| e.into_inner()).remove(&pod_id);
}

/// Protocol errors with a request id fail that request at once.
#[sqlx::test]
async fn test_a_protocol_error_fails_its_request_at_once(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let mut agent = fake_agent(pod_id, current(), Duration::ZERO).await;
    let conn = reconnect_if_needed(&pool, pod_id).await.expect("connects");
    let mut fake = agent.next_connection().await;
    let started = tokio::time::Instant::now();
    let waiting = conn.request(|request_id| ClientMessage::ListDirectory { request_id, path: "/".into() });
    let answer = async {
        let ClientMessage::ListDirectory { request_id, .. } = fake.next_request().await else { panic!() };
        let _ = fake.send.send(Out::Message(AgentMessage::ProtocolError {
            request_id: Some(request_id),
            message: "missing field `path`".into(),
        }));
    };
    let (result, ()) = tokio::join!(waiting, answer);
    assert!(matches!(&result, Err(AgentRequestError::Rejected(m)) if m.contains("missing field")), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(3));
}

/// An event from a newer agent that smelt doesn't know is skipped, and
/// the connection carries on.
#[sqlx::test]
async fn test_an_unknown_event_is_ignored(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let mut agent = fake_agent(pod_id, current(), Duration::ZERO).await;
    let conn = reconnect_if_needed(&pool, pod_id).await.expect("connects");
    let mut fake = agent.next_connection().await;
    let waiting = conn.request(|request_id| ClientMessage::ListDirectory { request_id, path: "/".into() });
    let answer = async {
        let ClientMessage::ListDirectory { request_id, .. } = fake.next_request().await else { panic!() };
        let _ = fake.send.send(Out::Raw(r#"{"event":"terminal_resized","terminal_id":"1","rows":40}"#.into()));
        fake.reply(request_id, Reply::DirectoryListed { entries: vec![] });
    };
    let (result, ()) = tokio::join!(waiting, answer);
    assert_eq!(result, Ok(Reply::DirectoryListed { entries: vec![] }));
}

/// Two callers that need the agent at once share one connection.
#[sqlx::test]
async fn test_two_callers_connecting_at_once_make_one_connection(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let agent = fake_agent(pod_id, current(), Duration::from_millis(300)).await;
    let (a, b) = tokio::join!(reconnect_if_needed(&pool, pod_id), reconnect_if_needed(&pool, pod_id));
    let (a, b) = (a.expect("a connects"), b.expect("b connects"));
    assert!(Arc::ptr_eq(&a, &b), "the two callers got different connections");
    assert_eq!(agent.dials.load(Ordering::SeqCst), 1, "the pod was dialled more than once");
}

/// A pod torn down while its connection was being made keeps no
/// connection: nothing registered, and the socket closed.
#[sqlx::test]
async fn test_a_teardown_during_a_connect_leaves_no_connection(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let mut agent = fake_agent(pod_id, current(), Duration::from_millis(500)).await;
    let connecting = tokio::spawn({
        let pool = pool.clone();
        async move { reconnect_if_needed(&pool, pod_id).await.map(|_| ()) }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    // What `force_terminate_pod` does, without the cluster.
    deregister(pod_id);
    db::terminate_sandbox_pod(&pool, pod_id).await.expect("terminate the row");

    let result = connecting.await.expect("join");
    assert!(matches!(result, Err(TerminalError::NoPod)), "{result:?}");
    assert!(!registry_contains(pod_id), "a connection to a torn-down pod is registered");
    let fake = agent.next_connection().await;
    tokio::time::timeout(Duration::from_secs(3), fake.ended)
        .await
        .expect("the connection to the torn-down pod is still open")
        .ok();
}

/// An agent from before the protocol was versioned: its first message
/// isn't a hello. The pod is outdated for good, without a retry or crash
/// cleanup, and the next call fails without dialling.
#[sqlx::test]
async fn test_a_protocol_0_agent_is_outdated_for_good(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let v0_line = r#"{"id":"cmd-1","terminal_id":"3","stream":"stdout","seq":1,"data":"hi"}"#;
    let agent = fake_agent(pod_id, Greeting::Raw(v0_line.into()), Duration::ZERO).await;

    let err = reconnect_if_needed(&pool, pod_id).await.map(|_| ()).expect_err("outdated");
    assert!(matches!(err, TerminalError::AgentOutdated { found: None }), "{err:?}");
    let text = err.to_string();
    assert!(text.contains("terminate_pod") && text.contains("create_pod"), "{text}");
    assert_eq!(agent.dials.load(Ordering::SeqCst), 1, "an outdated agent was retried");
    assert!(db::sandbox_pod_is_live(&pool, pod_id).await.expect("live?"), "the pod was cleaned up as a crash");

    let again = reconnect_if_needed(&pool, pod_id).await.map(|_| ());
    assert!(matches!(again, Err(TerminalError::AgentOutdated { found: None })), "{again:?}");
    assert_eq!(agent.dials.load(Ordering::SeqCst), 1, "the second call dialled again");
}

#[sqlx::test]
async fn test_an_agent_on_another_major_is_outdated(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let next_major = ProtocolVersion { major: PROTOCOL_VERSION.major + 1, minor: 0 };
    let _agent = fake_agent(pod_id, Greeting::Hello(next_major), Duration::ZERO).await;
    let err = reconnect_if_needed(&pool, pod_id).await.map(|_| ()).expect_err("outdated");
    assert!(matches!(err, TerminalError::AgentOutdated { found: Some(v) } if v == next_major), "{err:?}");
    assert!(err.to_string().contains(&format!("protocol {next_major}")), "{err}");
}

/// Silence could be an old agent with nothing to say, or a slow link, so
/// it fails this attempt but isn't remembered.
#[sqlx::test]
async fn test_an_agent_that_says_nothing_is_outdated_but_not_for_good(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let agent = fake_agent(pod_id, Greeting::Nothing, Duration::ZERO).await;
    let err = reconnect_if_needed(&pool, pod_id).await.map(|_| ()).expect_err("outdated");
    assert!(matches!(err, TerminalError::AgentOutdated { found: None }), "{err:?}");
    let _ = reconnect_if_needed(&pool, pod_id).await;
    assert_eq!(agent.dials.load(Ordering::SeqCst), 2, "silence was remembered as outdated");
}

/// Any minor of smelt's major connects; a feature from a later minor
/// fails alone, asking for a restart.
#[sqlx::test]
async fn test_another_minor_connects_and_gates_only_newer_features(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let older = ProtocolVersion { major: PROTOCOL_VERSION.major, minor: 3 };
    let _agent = fake_agent(pod_id, Greeting::Hello(older), Duration::ZERO).await;
    let conn = reconnect_if_needed(&pool, pod_id).await.expect("another minor connects");
    assert_eq!(conn.agent_version, older);
    assert!(conn.require_minor(3).is_ok());
    let err = conn.require_minor(4).expect_err("minor 4 is newer than the agent");
    assert!(matches!(err, TerminalError::AgentTooOld { found, .. } if found == older), "{err:?}");
    assert!(err.to_string().contains("needs a newer sandbox agent"), "{err}");
}

/// The Sandboxes page and `list_pods` say what each pod's agent speaks.
#[sqlx::test]
async fn test_agent_status_follows_what_the_agent_said(pool: PgPool) {
    use crate::api::pods::AgentStatus;

    let (_, unconnected) = pod_row(&pool).await;
    let _a = fake_agent(unconnected, current(), Duration::ZERO).await;
    assert_eq!(agent_status(unconnected), None, "not connected yet, so unknown");

    let (_, same) = pod_row(&pool).await;
    let _b = fake_agent(same, current(), Duration::ZERO).await;
    reconnect_if_needed(&pool, same).await.expect("connects");
    assert_eq!(agent_status(same), Some(AgentStatus::Current { version: PROTOCOL_VERSION.to_string() }));

    let (_, newer) = pod_row(&pool).await;
    let newer_minor = ProtocolVersion { major: PROTOCOL_VERSION.major, minor: PROTOCOL_VERSION.minor + 1 };
    let _c = fake_agent(newer, Greeting::Hello(newer_minor), Duration::ZERO).await;
    reconnect_if_needed(&pool, newer).await.expect("connects");
    assert_eq!(agent_status(newer), Some(AgentStatus::Current { version: newer_minor.to_string() }));

    // An older minor can't be faked while smelt is at minor 0, so check
    // the classification itself.
    let older = ProtocolVersion { major: PROTOCOL_VERSION.major, minor: 2 };
    let current_minor_3 = ProtocolVersion { major: PROTOCOL_VERSION.major, minor: 3 };
    assert_eq!(
        classify_agent(older, current_minor_3),
        AgentStatus::RestartRecommended { version: older.to_string() }
    );

    let (_, old) = pod_row(&pool).await;
    let v0_line = r#"{"id":"c","terminal_id":"1","stream":"stdout","seq":1,"data":"x"}"#;
    let _d = fake_agent(old, Greeting::Raw(v0_line.into()), Duration::ZERO).await;
    let _ = reconnect_if_needed(&pool, old).await;
    assert_eq!(agent_status(old), Some(AgentStatus::RestartRequired { version: None }));
}

/// Every `smelt::sandbox` log line from every test in this binary, through
/// one process-wide subscriber (a per-thread one misses events whose
/// callsite another test's thread registered first).
fn captured_logs() -> &'static Arc<StdMutex<Vec<u8>>> {
    #[derive(Clone)]
    struct Writer(Arc<StdMutex<Vec<u8>>>);
    impl std::io::Write for Writer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    static LOGS: std::sync::OnceLock<Arc<StdMutex<Vec<u8>>>> = std::sync::OnceLock::new();
    LOGS.get_or_init(|| {
        let logs = Arc::new(StdMutex::new(Vec::new()));
        let writer = Writer(logs.clone());
        tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_env_filter(tracing_subscriber::EnvFilter::new("smelt::sandbox=trace"))
            .init();
        logs
    })
}

/// The agent's protocol error quotes serde's message, which can quote
/// the request it couldn't read (a `write_file`'s content). smelt logs
/// that it happened, not the text.
#[sqlx::test]
async fn test_a_protocol_error_is_not_logged_verbatim(pool: PgPool) {
    let logs = captured_logs();
    let (_, pod_id) = pod_row(&pool).await;
    let mut agent = fake_agent(pod_id, current(), Duration::ZERO).await;
    let conn = reconnect_if_needed(&pool, pod_id).await.expect("connects");
    let mut fake = agent.next_connection().await;
    let _ = fake.send.send(Out::Message(AgentMessage::ProtocolError {
        request_id: None,
        message: r#"invalid type: string "password=hunter2-SECRET", expected u32"#.into(),
    }));
    // A round trip after it, so the reader has handled the error.
    let waiting = conn.request(|request_id| ClientMessage::ListDirectory { request_id, path: "/".into() });
    let answer = async {
        let ClientMessage::ListDirectory { request_id, .. } = fake.next_request().await else { panic!() };
        fake.reply(request_id, Reply::DirectoryListed { entries: vec![] });
    };
    let (result, ()) = tokio::join!(waiting, answer);
    result.expect("the connection still works");

    let logged = String::from_utf8_lossy(&logs.lock().unwrap_or_else(|e| e.into_inner())).into_owned();
    assert!(logged.contains("couldn't read a message from smelt"), "nothing was logged: {logged:?}");
    assert!(!logged.contains("hunter2-SECRET"), "the protocol error's text was logged: {logged}");
}

/// A teardown whose delete fails leaves the pod running and listed as
/// live, so it must stay usable: a later connect registers.
#[sqlx::test]
async fn test_a_failed_teardown_leaves_the_pod_usable(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let _agent = fake_agent(pod_id, current(), Duration::ZERO).await;
    let failed = terminate_pod_with(&pool, pod_id, async {
        Err(SandboxError::StartFailed("the API server is down".into()))
    })
    .await;
    assert!(failed.is_err());
    assert!(db::sandbox_pod_is_live(&pool, pod_id).await.expect("live?"), "the row was closed");

    let conn = reconnect_if_needed(&pool, pod_id).await;
    assert!(conn.is_ok(), "the pod is unusable after a failed teardown: {:?}", conn.map(|_| ()));
    assert!(registry_contains(pod_id));
}

/// A connection whose socket closed before `connect_with_retry` got to
/// register it isn't registered: its reader has already given up on it,
/// and nothing would ever replace it.
#[sqlx::test]
async fn test_a_connection_that_already_ended_is_not_registered(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let mut agent = fake_agent(pod_id, current(), Duration::ZERO).await;
    let conn = connect(pool.clone(), pod_id, dialer_for(pod_id)).await.expect("connects");
    let fake = agent.next_connection().await;
    let _ = fake.send.send(Out::Close);
    tokio::time::timeout(Duration::from_secs(3), async {
        while conn.request(|request_id| ClientMessage::ListDirectory { request_id, path: "/".into() }).await
            != Err(AgentRequestError::Disconnected)
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the reader never noticed the close");

    assert!(register(pod_id, conn).is_err(), "a connection that already ended was registered");
    assert!(!registry_contains(pod_id));
}

/// A page load doesn't wait behind a connect that's already under way.
#[sqlx::test]
async fn test_try_reconnect_does_not_wait_for_a_connect_in_progress(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let _agent = fake_agent(pod_id, current(), Duration::from_secs(2)).await;
    let connecting = tokio::spawn({
        let pool = pool.clone();
        async move { reconnect_if_needed(&pool, pod_id).await.map(|_| ()) }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = tokio::time::Instant::now();
    try_reconnect(&pool, pod_id).await;
    assert!(started.elapsed() < Duration::from_millis(500), "try_reconnect waited {:?}", started.elapsed());
    connecting.await.expect("join").expect("the first connect still succeeds");
}
