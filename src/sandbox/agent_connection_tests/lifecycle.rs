use super::*;

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
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_connection_that_already_ended_is_not_registered(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let mut agent = fake_agent(pod_id, current(), Duration::ZERO).await;
    let conn = connect(pool.clone(), pod_id, dialer_for(pod_id, test_instance()).expect("the test's fake dialer")).await.expect("connects");
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
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

/// Review 2: the Sandboxes page's refresh (`try_reconnect`) reads an
/// unknown image again too, so the page doesn't say "restart" where only
/// a rebuild helps; it still never dials the refused agent again.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_page_refresh_reads_an_unknown_image_again(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let v0_line = r#"{"id":"cmd-1","terminal_id":"3","stream":"stdout","seq":1,"data":"hi"}"#;
    let agent = fake_agent(pod_id, Greeting::Raw(v0_line.into()), Duration::ZERO).await;
    let _ = reconnect_if_needed(&pool, pod_id).await;
    assert_eq!(agent_status(pod_id), Some(AgentStatus::RestartRequired { version: None, rebuild_image: false }));

    fake_images().lock().unwrap_or_else(|e| e.into_inner()).insert(pod_id, default_sandbox_image());
    try_reconnect(&pool, pod_id).await;
    assert_eq!(agent_status(pod_id), Some(AgentStatus::RestartRequired { version: None, rebuild_image: true }));
    try_reconnect(&pool, pod_id).await;
    assert_eq!(agent.dials.load(Ordering::SeqCst), 1, "the refused agent was dialled again");
}

/// Review 2: a pod torn down while its refused agent's image is read
/// isn't remembered as outdated afterwards (nothing would ever clear it).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_pod_torn_down_during_the_image_read_isnt_remembered(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let (reading, release) = (Arc::new(tokio::sync::Notify::new()), Arc::new(tokio::sync::Notify::new()));
    image_reads_held().lock().unwrap_or_else(|e| e.into_inner()).insert(pod_id, (reading.clone(), release.clone()));
    let v0_line = r#"{"id":"cmd-1","terminal_id":"3","stream":"stdout","seq":1,"data":"hi"}"#;
    let _agent = fake_agent(pod_id, Greeting::Raw(v0_line.into()), Duration::ZERO).await;

    let connecting = tokio::spawn({
        let pool = pool.clone();
        async move { reconnect_if_needed(&pool, pod_id).await.map(|_| ()) }
    });
    tokio::time::timeout(Duration::from_secs(5), reading.notified()).await.expect("the image read started");
    deregister(pod_id);
    release.notify_one();
    let result = connecting.await.expect("join");
    assert!(matches!(result, Err(TerminalError::AgentOutdated { .. })), "{result:?}");
    assert_eq!(outdated(pod_id), None, "a torn-down pod is remembered as outdated");
    assert_eq!(agent_status(pod_id), None);
}

/// Review 2: `index.docker.io` is Docker Hub, a first part with capitals
/// is a registry (as Docker's reference parser takes it), and spaces
/// around a setting don't make it another image.
#[test]
fn test_full_image_name_follows_dockers_reference_rules() {
    assert_eq!(full_image_name("index.docker.io/library/smelt-sandbox:latest"), "docker.io/library/smelt-sandbox:latest");
    assert_eq!(full_image_name("index.docker.io/smelt-sandbox"), "docker.io/library/smelt-sandbox:latest");
    assert_eq!(full_image_name("Registry/team/sandbox:1"), "Registry/team/sandbox:1");
    assert_eq!(full_image_name(" smelt-sandbox:latest\n"), "docker.io/library/smelt-sandbox:latest");
    assert_eq!(full_image_name("localhost:5000/sandbox"), "localhost:5000/sandbox:latest");
    assert_eq!(full_image_name("smelt-sandbox@sha256:abc"), "docker.io/library/smelt-sandbox@sha256:abc");
}

/// Review 2: the rebuild advice for a pod on the image a new pod gets
/// names the way to get that image, as the missing-image advice does: the
/// build script makes only this tree's own image, and `:latest` only with
/// `--latest`.
#[test]
fn test_the_rebuild_advice_fits_the_image() {
    let advice = |image: &str| {
        TerminalError::AgentOutdated { found: None, image: PodImage::SameAsNew(image.to_string()) }.to_string()
    };
    let own = advice(OWN_SANDBOX_IMAGE);
    assert!(own.contains("with scripts/build-sandbox-image.sh") && !own.contains("--latest"), "{own}");
    let latest = advice("smelt-sandbox:latest");
    assert!(latest.contains("scripts/build-sandbox-image.sh --latest"), "{latest}");
    let other = advice("registry.example/team/sandbox:1");
    assert!(other.contains("SANDBOX_IMAGE or SANDBOX_DOCKER_IMAGE names it"), "{other}");
    assert!(!other.contains("with scripts/build-sandbox-image.sh"), "{other}");
    for text in [own, latest, other] {
        assert!(text.contains("Once it's rebuilt, call terminate_pod, then create_pod"), "{text}");
    }
}

/// The `SandboxCommandUpdate`s `events` carries for `command_id`, as
/// (started?, status, exit code), until one that isn't `running` or
/// `deadline` passes.
async fn command_updates(
    events: &mut tokio::sync::broadcast::Receiver<crate::events::ConversationEvent>,
    command_id: &str,
    deadline: Duration,
) -> Vec<(bool, String, Option<i32>)> {
    let mut seen = Vec::new();
    let _ = tokio::time::timeout(deadline, async {
        loop {
            match events.recv().await {
                Ok(crate::events::ConversationEvent::SandboxCommandUpdate {
                    command_id: id, command, status, exit_code, ..
                }) if id == command_id => {
                    let done = status != "running";
                    seen.push((command.is_some(), status, exit_code));
                    if done {
                        return;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return,
            }
        }
    })
    .await;
    seen
}

/// SME-144: a command the agent finishes at once (`true`) is announced as
/// started before its finish, so an open tab doesn't apply the finish to
/// the previous command and then show this one running for good. The
/// start is published before the send, so no scheduling between the send
/// and the publish can put the finish first.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_command_is_announced_before_its_finish(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let (conversation_id, pod_id) = pod_row(&pool).await;
    let terminal = db::create_sandbox_terminal(&pool, pod_id).await.expect("terminal row");
    let mut agent = fake_agent(pod_id, current(), Duration::ZERO).await;
    let mut events = crate::events::subscribe(conversation_id);

    let run = tokio::spawn({
        let pool = pool.clone();
        async move { run_command(&pool, conversation_id, terminal.id, "cmd-fast", "true", 300).await }
    });
    let mut fake = agent.next_connection().await;
    let ClientMessage::Command { id, terminal_id, .. } = fake.next_request().await else {
        panic!("expected the command");
    };
    let _ = fake.send.send(Out::Message(AgentMessage::Exit { id, terminal_id, code: 0 }));
    run.await.expect("join").expect("the command was sent");

    let updates = command_updates(&mut *events, "cmd-fast", Duration::from_secs(5)).await;
    assert_eq!(
        updates,
        vec![(true, "running".to_string(), None), (false, "finished".to_string(), Some(0))],
        "the start must come before the finish"
    );
}

/// SME-144: a send that fails marks the command lost and says so to open
/// tabs, after announcing it, so the panel shows `lost` live rather than
/// the previous command's status.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_failed_send_announces_the_command_lost(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let (conversation_id, pod_id) = pod_row(&pool).await;
    let terminal = db::create_sandbox_terminal(&pool, pod_id).await.expect("terminal row");
    // An agent on another major: the connect, and so the send, fails.
    let next_major = ProtocolVersion { major: PROTOCOL_VERSION.major + 1, minor: 0 };
    let _agent = fake_agent(pod_id, Greeting::Hello(next_major), Duration::ZERO).await;
    let mut events = crate::events::subscribe(conversation_id);

    let result = run_command(&pool, conversation_id, terminal.id, "cmd-unsent", "echo hi", 300).await;
    assert!(matches!(result, Err(RunCommandError::Failed(_))), "{result:?}");

    let updates = command_updates(&mut *events, "cmd-unsent", Duration::from_secs(3)).await;
    assert_eq!(updates, vec![(true, "running".to_string(), None), (false, "lost".to_string(), None)]);
    let row = db::get_terminal_command(&pool, "cmd-unsent").await.expect("read").expect("the row");
    assert_eq!(row.status, "lost");
}

/// SME-144 review 1: when the send fails and marking the row lost fails
/// too, the row still says running (and the terminal stays busy), so open
/// tabs aren't told `lost`: a reload would show running again.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_lost_mark_that_fails_announces_nothing(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let (conversation_id, pod_id) = pod_row(&pool).await;
    let terminal = db::create_sandbox_terminal(&pool, pod_id).await.expect("terminal row");
    // This test database refuses to mark any command lost.
    sqlx::query(
        "CREATE FUNCTION refuse_lost() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF NEW.status = 'lost' THEN RAISE EXCEPTION 'refused for the test'; END IF; RETURN NEW; END $$",
    )
    .execute(&pool)
    .await
    .expect("create the trigger function");
    sqlx::query("CREATE TRIGGER refuse_lost BEFORE UPDATE ON terminal_commands FOR EACH ROW EXECUTE FUNCTION refuse_lost()")
        .execute(&pool)
        .await
        .expect("create the trigger");
    let next_major = ProtocolVersion { major: PROTOCOL_VERSION.major + 1, minor: 0 };
    let _agent = fake_agent(pod_id, Greeting::Hello(next_major), Duration::ZERO).await;
    let mut events = crate::events::subscribe(conversation_id);

    let result = run_command(&pool, conversation_id, terminal.id, "cmd-unmarked", "echo hi", 300).await;
    assert!(matches!(result, Err(RunCommandError::Failed(_))), "{result:?}");

    let updates = command_updates(&mut *events, "cmd-unmarked", Duration::from_secs(2)).await;
    assert_eq!(updates, vec![(true, "running".to_string(), None)], "lost was announced though the row says running");
    let row = db::get_terminal_command(&pool, "cmd-unmarked").await.expect("read").expect("the row");
    assert_eq!(row.status, "running");
}
