use super::*;

/// Docker in a real sandbox pod behaves like Docker on a Linux machine,
/// and stays inside the pod (SME-33). One test so the pods, the claim
/// and the base image are made once.
#[tokio::test]
async fn test_docker_in_a_sandbox_pod_works_and_stays_inside_the_pod() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let pods = pods_api(&client);
    let conversation_id = (uuid_like().parse::<u128>().expect("nanos") % 1_000_000_000) as i64
        + 1_000_000_000;
    let docker = DockerSidecar {
        memory: "1Gi".to_string(),
        storage: PodStorage::Conversation(conversation_id),
    };
    ensure_conversation_pvcs(&client, conversation_id, &test_instance()).await.expect("ensure docker claim");
    // Every pod this test makes, so cleanup below finds them even after
    // a failed assertion unwinds out of the checks.
    let created: StdMutex<Vec<String>> = StdMutex::new(Vec::new());

    let checks = tokio::time::timeout(Duration::from_secs(300), async {
        let first = manager
            .create_with_docker(&unique_session_id("docker"), "256Mi", &docker, &[], &[], &test_instance())
            .await
            .expect("create pod with docker");
        created.lock().expect("created").push(first.pod_name.clone());

        // The very first command works: the pod only runs once dockerd answers.
        let info = first
            .exec(&["docker", "info", "--format", "{{.ServerVersion}}"])
            .await
            .expect("exec docker info");
        assert_eq!(info.exit_code, 0, "docker info right after create: {}", info.stdout);
        assert!(info.stdout.starts_with("29."), "server version: {}", info.stdout);

        // A base image from the sandbox's own files: no Docker Hub.
        let import = first
            .exec(&[
                "bash",
                "-c",
                "sudo tar -C / -c bin sbin lib lib64 usr etc 2>/dev/null | docker import - local/base",
            ])
            .await
            .expect("exec import");
        assert_eq!(import.exit_code, 0, "import base image: {}", import.stdout);

        let run = first
            .exec(&["docker", "run", "--rm", "local/base", "echo", "hello-from-docker"])
            .await
            .expect("exec docker run");
        assert_eq!(run.stdout.trim(), "hello-from-docker");

        // A relative bind mount from /workspace sees the sandbox's files.
        let compose = first
            .exec(&[
                "bash",
                "-c",
                "mkdir -p /workspace/proj/src && echo from-workspace > /workspace/proj/src/f \
                     && cd /workspace/proj \
                     && printf 'services:\\n  web:\\n    image: local/base\\n    command: [cat, /data/f]\\n    volumes: [./src:/data]\\n' > compose.yml \
                     && docker compose run --rm -q web 2>&1",
            ])
            .await
            .expect("exec compose");
        assert!(
            compose.stdout.contains("from-workspace"),
            "compose bind mount from /workspace: {}",
            compose.stdout
        );

        // Nested containers live under the sidecar's own cgroup, so its
        // memory limit applies, never at the node's cgroup root.
        let started = first
            .exec(&["docker", "run", "-d", "local/base", "sleep", "300"])
            .await
            .expect("exec docker run -d");
        let id = started.stdout.trim().to_string();
        let where_ = first
            .exec_in(
                "docker",
                &[
                    "sh",
                    "-c",
                    &format!(
                        "own=$(dirname $(sed -n 's/^0:://p' /proc/1/cgroup)); \
                             pid=$(docker --host={DOCKER_HOST} inspect -f '{{{{.State.Pid}}}}' {id}); \
                             echo \"$own\"; sed -n 's/^0:://p' /proc/$pid/cgroup"
                    ),
                ],
            )
            .await
            .expect("exec in docker sidecar");
        let mut lines = where_.stdout.lines();
        let own = lines.next().unwrap_or_default().to_string();
        let nested = lines.next().unwrap_or_default().to_string();
        assert!(
            !own.is_empty() && nested.starts_with(&format!("{own}/docker/")),
            "nested container cgroup {nested:?} should be under the sidecar's {own:?}"
        );

        // Both browsers' routes into the pod (SME-33): a published port
        // on localhost, any container port at its address through the
        // agent's relay, and nothing outside the Docker range.
        let serve = first
            .exec(&[
                "bash",
                "-c",
                "mkdir -p /workspace/www && echo from-a-container > /workspace/www/index.html \
                     && docker run -d -p 8000:8000 -v /workspace/www:/w -w /w local/base python3 -m http.server 8000 >/dev/null \
                     && docker run -d --name unpublished -v /workspace/www:/w -w /w local/base python3 -m http.server 8001 >/dev/null \
                     && sleep 2 && docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' unpublished",
            ])
            .await
            .expect("exec start servers");
        let container_ip: std::net::Ipv4Addr =
            serve.stdout.trim().parse().unwrap_or_else(|_| panic!("container ip: {:?}", serve.stdout));
        let fetch = |host: PodHost, port: u16| {
            let client = client.clone();
            let pod = first.pod_name.clone();
            async move {
                use tokio::io::AsyncWriteExt;
                let mut stream = dial_pod(&client, &pod, host, port).await.expect("dial_pod");
                stream.write_all(b"GET / HTTP/1.0\r\n\r\n").await.expect("send request");
                let mut reply = String::new();
                let _ = tokio::time::timeout(Duration::from_secs(10), stream.read_to_string(&mut reply)).await;
                reply
            }
        };
        assert!(fetch(PodHost::Localhost, 8000).await.contains("from-a-container"), "published port on localhost");
        assert!(
            fetch(PodHost::Container(container_ip), 8001).await.contains("from-a-container"),
            "unpublished container port through the relay"
        );
        assert_eq!(
            fetch(PodHost::Container(std::net::Ipv4Addr::new(10, 43, 0, 1)), 443).await,
            "",
            "the relay must refuse an address outside the Docker range"
        );
        // An address no container has yet (one still starting) isn't
        // listening: the relay mustn't look open while it's connecting.
        let nobody = dial_pod(&client, &first.pod_name, PodHost::Container(std::net::Ipv4Addr::new(172, 21, 200, 200)), 8001)
            .await
            .expect("dial_pod");
        assert!(!stream_is_listening(nobody).await, "nothing is at 172.21.200.200:8001");
        assert!(
            stream_is_listening(dial_pod(&client, &first.pod_name, PodHost::Container(container_ip), 8001).await.expect("dial_pod")).await,
            "the unpublished container is listening"
        );

        // A container can't reach the sandbox agent, whose WebSocket
        // runs commands with no login, at the bridge gateway.
        let agent = first
            .exec(&[
                "docker",
                "run",
                "--rm",
                "local/base",
                "bash",
                "-c",
                &format!("</dev/tcp/{}/{AGENT_PORT}", DOCKER_BRIDGE_IP.split('/').next().unwrap_or_default()),
            ])
            .await
            .expect("exec agent probe");
        assert_ne!(agent.exit_code, 0, "a container reached the sandbox agent at the gateway");

        // dockerd never listens on TCP.
        for port in [2375, 2376] {
            let tcp = first
                .exec(&["bash", "-c", &format!("</dev/tcp/127.0.0.1/{port}")])
                .await
                .expect("exec tcp probe");
            assert_ne!(tcp.exit_code, 0, "nothing should listen on {port}");
        }

        // Images live on the conversation's claim, so a new pod has them.
        let first_name = first.pod_name.clone();
        pods.delete(&first_name, &immediate_delete_params()).await.ok();
        std::mem::forget(first);
        while pods.get_opt(&first_name).await.ok().flatten().is_some() {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let second = manager
            .create_with_docker(&unique_session_id("docker"), "256Mi", &docker, &[], &[], &test_instance())
            .await
            .expect("recreate pod on the same claim");
        created.lock().expect("created").push(second.pod_name.clone());
        let images = second
            .exec(&["docker", "image", "ls", "-q", "local/base"])
            .await
            .expect("exec image ls");
        assert!(!images.stdout.trim().is_empty(), "local/base should survive a new pod");
        std::mem::forget(second);
    });
    let outcome = std::panic::AssertUnwindSafe(checks).catch_unwind().await;

    // Delete what this test made directly, not through the cleanup queue,
    // whether or not the checks passed.
    let created = created.into_inner().expect("created");
    for name in &created {
        pods.delete(name, &immediate_delete_params()).await.ok();
    }
    for name in &created {
        while pods.get_opt(name).await.ok().flatten().is_some() {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
    pvc_api(&client)
        .delete(&docker_pvc_name(conversation_id), &DeleteParams::default())
        .await
        .ok();
    match outcome {
        Ok(finished) => finished.expect("docker test should finish within the timeout"),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// An OOM kill inside a nested container restarts only the Docker
/// sidecar: the sandbox container, its processes and /workspace carry
/// on, and the pod's status says why, as `docker_restart_to_report`
/// reads it (SME-33's spike). Written memory, not `bytearray(n)`, whose
/// untouched pages never count.
#[tokio::test]
async fn test_an_oom_in_a_nested_container_restarts_only_the_docker_sidecar() {
    let client = test_client().await;
    let manager = SandboxManager::new(client.clone());
    let pods = pods_api(&client);
    let docker = DockerSidecar {
        memory: "512Mi".to_string(),
        storage: PodStorage::Ephemeral,
    };
    let sandbox = manager
        .create_with_docker(&unique_session_id("docker-oom"), "128Mi", &docker, &[], &[], &test_instance())
        .await
        .expect("create pod");
    let name = sandbox.pod_name.clone();

    let checks = std::panic::AssertUnwindSafe(tokio::time::timeout(Duration::from_secs(240), async {
        let setup = sandbox
            .exec(&[
                "bash",
                "-c",
                "echo kept > /workspace/marker && (setsid sleep 1000 >/dev/null 2>&1 &) \
                     && sudo tar -C / -c bin sbin lib lib64 usr etc 2>/dev/null \
                        | docker import - local/base >/dev/null && echo ready",
            ])
            .await
            .expect("exec setup");
        assert_eq!(setup.stdout.trim(), "ready", "setup: {}", setup.stdout);
        // Detached: the container's memory outgrows the sidecar's limit.
        let started = sandbox
            .exec(&["docker", "run", "-d", "local/base", "sh", "-c", "head -c 900M /dev/zero | tail; sleep 600"])
            .await
            .expect("exec docker run");
        let container = started.stdout.trim().to_string();

        // Read as `watch_pods` reads it: every poll goes through
        // `note_docker_restarts`, and a restart no update explains yet
        // is re-read after `DOCKER_REASON_WAIT` (SME-85). The test pod
        // isn't named like a smelt pod, so it's watched as one here.
        let as_watched = |pod: Pod| named(pod, "sandbox-1");
        let mut seen = HashMap::new();
        let first = pods.get(&name).await.expect("get pod");
        note_docker_restarts(&mut seen, &as_watched(first), true);
        let restarted = loop {
            let pod = pods.get(&name).await.expect("get pod");
            let restarts = |pod: &Pod| docker_status(pod).map(|c| c.restart_count);
            match note_docker_restarts(&mut seen, &as_watched(pod.clone()), false) {
                Some(DockerRestart::Report { reason, .. }) => {
                    let count = restarts(&pod);
                    break (pod, (count, Some(reason)));
                }
                Some(DockerRestart::Recheck { .. }) => {
                    tokio::time::sleep(DOCKER_REASON_WAIT).await;
                    let pod = pods.get(&name).await.expect("re-read pod");
                    let reason = docker_restart_to_report(&pod, 0).and_then(|(_, reason)| reason);
                    let count = restarts(&pod);
                    break (pod, (count, reason));
                }
                None => {}
            }
            // A workload that exits on its own never reaches the limit:
            // say why now rather than at the timeout. Exit code 255 is
            // not that: it's how a restarted dockerd reports the
            // containers its OOM-killed predecessor ran, often before
            // the pod's status shows the restart.
            let state = sandbox
                .exec(&["docker", "inspect", "-f", "{{.State.Status}} {{.State.ExitCode}}", &container])
                .await
                .expect("exec docker inspect");
            if state.stdout.trim().starts_with("exited") && state.stdout.trim() != "exited 255" {
                let why = sandbox
                    .exec(&["sh", "-c", &format!("docker inspect -f '{{{{json .State}}}}' {container}; docker logs {container} 2>&1 | tail -5")])
                    .await
                    .expect("exec docker inspect");
                panic!("the workload exited without an OOM kill: {}", why.stdout);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        let (pod, report) = restarted;
        assert_eq!(report, (Some(1), Some("OOMKilled".to_string())));
        let sandbox_restarts = pod
            .status
            .and_then(|s| s.container_statuses)
            .and_then(|cs| cs.into_iter().find(|c| c.name == "sandbox"))
            .map(|c| c.restart_count);
        assert_eq!(sandbox_restarts, Some(0), "the sandbox container must not restart");

        let after = sandbox
            .exec(&["bash", "-c", "cat /workspace/marker; pgrep -x sleep >/dev/null && echo sleep-alive || \
                        (for p in /proc/[0-9]*; do [ \"$(cat $p/comm 2>/dev/null)\" = sleep ] && echo sleep-alive && break; done)"])
            .await
            .expect("exec after restart");
        assert!(after.stdout.contains("kept"), "/workspace should survive: {}", after.stdout);
        assert!(after.stdout.contains("sleep-alive"), "sandbox processes should survive: {}", after.stdout);
    }))
    .catch_unwind()
    .await;

    pods.delete(&name, &immediate_delete_params()).await.ok();
    std::mem::forget(sandbox);
    while pods.get_opt(&name).await.ok().flatten().is_some() {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    match checks {
        Ok(finished) => finished.expect("the sidecar should be OOM-killed within the timeout"),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
