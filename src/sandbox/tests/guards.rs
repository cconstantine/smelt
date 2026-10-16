use super::*;

#[test]
fn test_check_reachable_port_refuses_both_of_the_agents_ports() {
    assert!(matches!(check_reachable_port(AGENT_PORT), Err(TerminalError::AgentPort(8088))));
    assert!(matches!(check_reachable_port(RELAY_PORT), Err(TerminalError::AgentPort(8089))));
    assert!(check_reachable_port(3000).is_ok());
}

#[test]
fn test_a_missing_stream_is_a_no_stream_error_naming_it() {
    let err = require_stream::<u8>(None, "the exec's stdout").expect_err("no stream should be an error");
    assert!(
        matches!(&err, SandboxError::NoStream(what) if what == "the exec's stdout"),
        "expected NoStream naming the stream, got {err:?}"
    );
    assert_eq!(err.to_string(), "kube gave no stream for the exec's stdout");
    assert_eq!(
        require_stream(Some(7u8), "the exec's stdout").expect("a stream kube gave is passed through"),
        7
    );
}

#[test]
fn test_an_io_error_says_it_was_on_an_exec_or_port_forward_stream() {
    let err = SandboxError::Io(std::io::Error::other("broken pipe"));
    assert_eq!(err.to_string(), "I/O error on an exec's or port-forward's stream: broken pipe");
}

#[test]
fn test_check_pod_guard_allows_when_no_live_pod_exists() {
    assert!(check_pod_guard(&[]).is_ok());
}

#[test]
fn test_check_pod_guard_refuses_when_a_live_pod_already_exists() {
    let result = check_pod_guard(&[fake_pod(1)]);
    assert!(
        matches!(result, Err(SandboxError::PodAlreadyExists)),
        "expected PodAlreadyExists, got {result:?}"
    );
}

#[test]
fn test_resolve_pod_id_returns_the_one_live_pod() {
    assert_eq!(resolve_pod_id(&[fake_pod(42)]).expect("should resolve"), 42);
}

#[test]
fn test_resolve_pod_id_errors_with_no_pod_when_none_live() {
    let result = resolve_pod_id(&[]);
    assert!(
        matches!(result, Err(TerminalError::NoPod)),
        "expected NoPod, got {result:?}"
    );
}

/// DB-only — `create_pod`'s guard must fire *before* it ever touches
/// the process-global `MANAGER` (unset here, since this test doesn't
/// need a real cluster), so a refusal shows up as `PodAlreadyExists`,
/// not a panic from `get()`.
#[sqlx::test]
async fn test_create_pod_refuses_before_touching_the_manager_when_a_live_pod_exists(
    pool: PgPool,
) {
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    db::create_sandbox_pod(&pool, conversation.id)
        .await
        .expect("create sandbox pod");

    let result = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await;
    assert!(
        matches!(result, Err(SandboxError::PodAlreadyExists)),
        "expected PodAlreadyExists, got {result:?}"
    );
}

/// DB-only: with no live pod there's nothing to connect to, and the
/// manager is never touched.
#[sqlx::test]
async fn test_open_pod_port_without_a_live_pod_is_no_pod(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let result = open_pod_target(&pool, conversation.id, PodHost::Localhost, 3000).await;
    assert!(matches!(result, Err(TerminalError::NoPod)), "expected NoPod");
}

/// DB-only: the sandbox agent's port is refused before anything is
/// looked up, so no route can reach it.
#[sqlx::test]
async fn test_open_pod_port_refuses_the_sandbox_agents_own_port(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let result = open_pod_target(&pool, conversation.id, PodHost::Localhost, AGENT_PORT).await;
    assert!(matches!(result, Err(TerminalError::AgentPort(_))), "the agent's port must be refused");
}

/// Sends a bare HTTP request down `stream` and returns whatever comes
/// back before the response stops arriving — never waits for the end
/// of the stream, which a port-forward reports about a second late.
async fn http_get_over(stream: &mut Box<dyn PodIo>, path: &str) -> String {
    use tokio::io::AsyncWriteExt;
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
        .await
        .expect("write the request");
    let mut buf = vec![0u8; 4096];
    match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf)).await {
        Ok(Ok(n)) => String::from_utf8_lossy(&buf[..n]).to_string(),
        Ok(Err(e)) => panic!("read failed: {e}"),
        Err(_) => panic!("no response within 10s"),
    }
}

/// Real cluster: `open_pod_target` reaches a server inside the
/// conversation's own pod — one bound to `127.0.0.1` there, as dev
/// servers are by default — but never the sandbox agent's own port. A
/// conversation with no pod of its own can't borrow another's, and a
/// port nothing listens on ends at once with no bytes.
#[sqlx::test]
async fn test_open_pod_port_reaches_the_conversations_own_pod(pool: PgPool) {
    // Pod names are `sandbox-{pod_id}`, and every `#[sqlx::test]`
    // database restarts ids at 1 — start this one's ids far from the
    // low range.
    let first_id = (uuid_like().parse::<u128>().unwrap() % 1_000_000_000) as i64 + 1_000_000;
    sqlx::query("SELECT setval(pg_get_serial_sequence('sandbox_pods', 'id'), $1)")
        .bind(first_id)
        .execute(&pool)
        .await
        .expect("move the pod id sequence");
    // Its own client and manager, never the process-global one: a
    // manager set here would die with this test's runtime and break
    // every later test that uses `get()` (seen as `Kube(Service(Closed))`
    // on SME-42). The pod is created
    // under the name the database row gives it, as `create_pod` would.
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());

    let with_pod = db::create_conversation(&pool).await.expect("create conversation");
    let without_pod = db::create_conversation(&pool).await.expect("create conversation");
    let row = db::create_sandbox_pod(&pool, with_pod.id).await.expect("create the pod's row");
    // As this database's (SME-115): a port-forward goes only into a pod
    // labelled with the database's instance.
    let instance = db::smelt_instance(&pool).await.expect("instance");
    let docker = DockerSidecar { memory: "512Mi".to_string(), storage: PodStorage::Ephemeral };
    let sandbox = manager
        .create_with_docker(&row.id.to_string(), "128Mi", &docker, &[], &instance)
        .await
        .expect("create the pod");
    let open = |conversation_id: i64, port: u16| {
        let (client, pool) = (client.clone(), pool.clone());
        async move { open_pod_port_with(&pool, conversation_id, PodHost::Localhost, port, move || Ok(client)).await }
    };
    // Python's server speaks HTTP/1.0 and closes after every response,
    // so the preview below also meets an upstream that's gone between
    // requests on one kept-alive connection.
    let started_server = sandbox
        .exec(&["sh", "-c", "setsid nohup python3 -m http.server 8000 --bind 127.0.0.1 >/dev/null 2>&1 </dev/null &"])
        .await;

    // The server binds a moment after it starts. Everything is gathered
    // before asserting, so a failure still deletes the pod below
    // instead of leaving it in the cluster.
    let mut reply = Err("never tried".to_string());
    for _ in 0..50 {
        reply = match open(with_pod.id, 8000).await {
            Ok(mut stream) => Ok(http_get_over(&mut stream, "/no-such-path").await),
            Err(e) => Err(e.to_string()),
        };
        if reply.as_ref().is_ok_and(|r| !r.is_empty()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // The user's preview proxy over the same real port-forward: three
    // requests on one kept-alive connection. Each would take over a
    // second if anything waited for the port-forward's late end of
    // stream (see `open_pod_target`).
    let through_preview = {
        use crate::egress_proxy::{DialFuture, SandboxDial};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let preview_addr = listener.local_addr().expect("local addr");
        let template = crate::preview::PreviewTemplate::parse(&format!(
            "http://{{port}}-{{conversation}}.preview.localhost:{}",
            preview_addr.port()
        ))
        .expect("a template");
        let (dial_client, dial_pool) = (client.clone(), pool.clone());
        let dial_for: crate::preview::DialFor = Arc::new(move |conversation| {
            let (client, pool) = (dial_client.clone(), dial_pool.clone());
            Arc::new(move |_host, port| {
                let (client, pool) = (client.clone(), pool.clone());
                Box::pin(async move {
                    open_pod_port_with(&pool, conversation, PodHost::Localhost, port, move || Ok(client))
                        .await
                        .map_err(|e| crate::egress_proxy::DialError::from(e.to_string()))
                }) as DialFuture
            }) as SandboxDial
        });
        let server = tokio::spawn(crate::preview::serve(listener, template.clone(), dial_for, None));
        let http = reqwest::Client::builder()
            .resolve(&format!("8000-{}.preview.localhost", with_pod.id), preview_addr)
            .build()
            .expect("a client");
        let started = std::time::Instant::now();
        let mut statuses = Vec::new();
        for _ in 0..3 {
            match http.get(format!("{}/no-such-path", template.url_for(with_pod.id, PodHost::Localhost, 8000))).send().await {
                Ok(response) => statuses.push(response.status().as_u16()),
                Err(_) => statuses.push(0),
            }
        }
        server.abort();
        (statuses, started.elapsed())
    };
    let (probe_client, probe_pool) = (client.clone(), pool.clone());
    let server_listening =
        pod_port_is_listening_with(&probe_pool, with_pod.id, PodHost::Localhost, 8000, || Ok(probe_client.clone())).await;
    let agent_port_result = open(with_pod.id, AGENT_PORT).await;
    let unused_listening = pod_port_is_listening_with(&probe_pool, with_pod.id, PodHost::Localhost, 9, || Ok(probe_client.clone())).await;
    let without_pod_result = open(without_pod.id, 8000).await;
    let closed_reply = match open(with_pod.id, 9).await {
        Ok(mut stream) => Ok(http_get_over(&mut stream, "/").await),
        Err(e) => Err(e.to_string()),
    };
    pods_api(&client).delete(&pod_name(row.id), &immediate_delete_params()).await.ok();
    std::mem::forget(sandbox);

    started_server.expect("start a server in the pod");
    let reply = reply.expect("open the server's port");
    assert!(reply.starts_with("HTTP/1.0 404"), "expected the server's 404, got {reply:?}");
    assert!(
        matches!(agent_port_result, Err(TerminalError::AgentPort(_))),
        "the sandbox agent's port must be refused even with a pod"
    );
    assert!(
        matches!(without_pod_result, Err(TerminalError::NoPod)),
        "a conversation without a pod must not reach any pod"
    );
    assert_eq!(closed_reply.expect("open an unused port"), "", "nothing listens on port 9");
    assert!(server_listening.expect("probe the server's port"), "the server listens on 8000");
    assert!(!unused_listening.expect("probe port 9"), "nothing listens on port 9");
    let (statuses, elapsed) = through_preview;
    assert_eq!(statuses, vec![404, 404, 404], "the server's 404 through the preview each time");
    assert!(elapsed < Duration::from_secs(2), "three preview requests took {elapsed:?}");
}

/// DB-only — no MANAGER touch, since `conversation_pod_id` never calls
/// `get()`.
#[sqlx::test]
async fn test_conversation_pod_id_resolves_the_live_pod_and_errors_with_no_pod_otherwise(
    pool: PgPool,
) {
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    let before = conversation_pod_id(&pool, conversation.id).await;
    assert!(
        matches!(before, Err(TerminalError::NoPod)),
        "expected NoPod before any pod exists, got {before:?}"
    );

    let pod = db::create_sandbox_pod(&pool, conversation.id)
        .await
        .expect("create sandbox pod");
    let resolved = conversation_pod_id(&pool, conversation.id)
        .await
        .expect("should resolve");
    assert_eq!(resolved, pod.id);
}
