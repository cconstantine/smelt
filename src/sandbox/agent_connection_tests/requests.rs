use super::*;

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
