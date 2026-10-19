use super::*;

/// SME-11, SME-32: one live pod per conversation. A terminal can't open
/// before there's a pod; a new pod has the stored SSH key and git identity
/// and a `/workspace` the sandbox user owns; a second `create_pod` is
/// refused and one pod is listed, Running. `terminate_pod` is refused
/// while a terminal is open, works once it's closed, and finds no pod the
/// second time, as for a conversation that never had one.
#[sqlx::test]
async fn test_create_pod_sets_up_one_live_pod_per_conversation(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let id = conversation.id;
        let too_early = create_terminal(&pool, id).await;
        assert!(
            matches!(too_early, Err(TerminalError::NoPod)),
            "create_terminal before any pod exists should fail with NoPod, got {too_early:?}"
        );

        // A stored key and commit identity reach every new pod (SME-32).
        let key = crate::git::generate_key("lifecycle");
        db::create_ssh_key(&pool, "lifecycle", &key.public_key, &key.private_key).await.expect("store key");
        db::set_git_identity(
            &pool,
            &crate::git::GitIdentity { name: "Lifecycle Test".to_string(), email: "lifecycle@example.com".to_string() },
        )
        .await
        .expect("store identity");
        let pod = create_pod(&pool, id, PodLimitOverrides::default()).await.expect("create_pod");
        let git_check = exec_with(
            &client,
            &pod_name(pod),
            "sandbox",
            &["sh", "-c", "test -s /etc/smelt/keys/lifecycle && git config user.email"],
            None,
        )
        .await
        .expect("exec git check");
        assert_eq!(
            (git_check.exit_code, git_check.stdout.trim()),
            (0, "lifecycle@example.com"),
            "a new pod has the stored key and identity"
        );
        // /workspace is the conversation's claim, whose root the
        // provisioner owns; the sandbox user gets it (SME-32 code review 2,
        // finding 6).
        let owner = exec_with(&client, &pod_name(pod), "sandbox", &["stat", "-c", "%U:%G", "/workspace"], None)
            .await
            .expect("exec stat");
        assert_eq!(owner.stdout.trim(), "sandbox:sandbox", "/workspace belongs to the sandbox user");

        let duplicate = create_pod(&pool, id, PodLimitOverrides::default()).await;
        assert!(
            matches!(duplicate, Err(SandboxError::PodAlreadyExists)),
            "create_pod should refuse a second live pod for the same conversation, got {duplicate:?}"
        );
        let pods_listed = list_pods(&pool, id).await.expect("list_pods should succeed");
        assert_eq!(pods_listed.len(), 1, "exactly the one pod should be listed");
        assert_eq!(pods_listed[0].status, "Running");

        let terminal = create_terminal(&pool, id).await.expect("create_terminal");
        let blocked = terminate_pod(&pool, id).await;
        assert!(
            matches!(blocked, Err(TerminalError::TerminalStillExists)),
            "terminate_pod should refuse while a terminal exists, got {blocked:?}"
        );
        terminate_terminal(&pool, terminal).await.expect("terminate_terminal");
        terminate_pod(&pool, id).await.expect("terminate_pod should succeed once its terminal is gone");
        // Resolved through the conversation's live pod, so a second call
        // finds none, as for a conversation that never had one.
        let repeat = terminate_pod(&pool, id).await;
        assert!(
            matches!(repeat, Err(TerminalError::NoPod)),
            "a second terminate_pod should find no pod, got {repeat:?}"
        );
        // A conversation that never had a pod, and an id with no
        // conversation at all.
        let never = db::create_conversation(&pool).await.expect("create conversation");
        for unknown_id in [never.id, 999_999_999] {
            let unknown = terminate_pod(&pool, unknown_id).await;
            assert!(
                matches!(unknown, Err(TerminalError::NoPod)),
                "terminate_pod on {unknown_id}, which never had a pod, should be NoPod, got {unknown:?}"
            );
        }
    })
    .await;
}

/// Terminals in one pod: each `create_terminal` makes a distinct one,
/// listed and connected. They keep their own working directories, a long
/// command in one doesn't hold up the other, and one can't be closed while
/// a command runs in it. Closing one leaves the other, and closing it again
/// is fine. Command history is per terminal, and outlives it.
#[sqlx::test]
async fn test_terminals_in_one_pod_run_independently_and_are_guarded(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let id = conversation.id;
        let pod = create_pod(&pool, id, PodLimitOverrides::default()).await.expect("create_pod");
        let terminal_1 = create_terminal(&pool, id).await.expect("create_terminal (1)");
        let terminal_2 = create_terminal(&pool, id).await.expect("create_terminal (2)");
        assert_ne!(terminal_1, terminal_2, "every create_terminal call should make a distinct terminal");
        let listed = list_terminals(&pool, id).await.expect("list_terminals");
        assert_eq!(listed.len(), 2, "both terminals in the one pod should be listed");
        assert!(listed.iter().all(|t| t.status == "connected"));
        assert!(listed.iter().all(|t| t.pod_id == pod));

        // Their own working directories.
        run_and_wait(&pool, id, terminal_1, "cd-1", "cd /tmp").await;
        run_and_wait(&pool, id, terminal_2, "cd-2", "cd /var").await;
        let pwd_1 = run_and_wait(&pool, id, terminal_1, "pwd-1", "pwd").await;
        let pwd_2 = run_and_wait(&pool, id, terminal_2, "pwd-2", "pwd").await;
        assert_eq!(first_stdout_line(&pool, &pwd_1).await, "/tmp");
        assert_eq!(first_stdout_line(&pool, &pwd_2).await, "/var");

        // A long command in one doesn't hold up the other.
        db::create_terminal_command(&pool, id, terminal_1, "long-1", "sleep 5", 300).await.expect("create_terminal_command");
        send_command(&pool, terminal_1, "long-1", "sleep 5").await.expect("send_command");
        tokio::time::sleep(Duration::from_millis(300)).await; // let it start
        db::create_terminal_command(&pool, id, terminal_2, "quick-2", "echo still_alive", 300)
            .await
            .expect("create_terminal_command");
        send_command(&pool, terminal_2, "quick-2", "echo still_alive").await.expect("send_command");
        let quick_status = poll_until_finished(&pool, "quick-2").await;
        assert_eq!(
            quick_status.status, "finished",
            "terminal 2 should finish a command while terminal 1's sleep is still running"
        );
        send_signal(&pool, terminal_1, "long-1", "KILL").await.expect("send_signal");
        poll_until_finished(&pool, "long-1").await;

        // terminate_terminal is refused while a command runs in it.
        db::create_terminal_command(&pool, id, terminal_2, "blocking-2", "sleep 30", 300).await.expect("create_terminal_command");
        send_command(&pool, terminal_2, "blocking-2", "sleep 30").await.expect("send_command");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let blocked_terminate = terminate_terminal(&pool, terminal_2).await;
        assert!(
            matches!(blocked_terminate, Err(TerminalError::CommandStillRunning)),
            "terminate_terminal should refuse while a command is running, got {blocked_terminate:?}"
        );
        send_signal(&pool, terminal_2, "blocking-2", "KILL").await.expect("send_signal");
        poll_until_finished(&pool, "blocking-2").await;

        // Closing terminal 2 leaves terminal 1 as it was; closing it again
        // is fine.
        terminate_terminal(&pool, terminal_2).await.expect("terminate_terminal should succeed once no command is running");
        terminate_terminal(&pool, terminal_2).await.expect("terminate_terminal should be idempotent");
        let still_pwd = run_and_wait(&pool, id, terminal_1, "pwd-1-again", "pwd").await;
        assert_eq!(
            first_stdout_line(&pool, &still_pwd).await,
            "/tmp",
            "terminal 1 should be unaffected by closing its sibling"
        );

        // Command history is per terminal, and outlives it.
        let history_2 = db::list_terminal_commands(&pool, terminal_2, 10)
            .await
            .expect("list_terminal_commands should still work for a closed terminal");
        assert!(history_2.iter().any(|c| c.command_id == "blocking-2"));
        assert!(
            !history_2.iter().any(|c| c.command_id == "cd-1"),
            "list_commands should not leak another terminal's history"
        );
    })
    .await;
}

/// SME-157: containers made by a pod's Docker sidecar get the pod's own
/// uplink MTU, not Docker's default of 1500 — on the default bridge and on
/// a user-defined bridge created after dockerd came up (compose's project
/// networks' equivalent). The pod's uplink is smaller than 1500 on these
/// clusters (Flannel's VXLAN, k3s's default), and a container advertising
/// MSS 1460 then loses every full-MSS segment its peer sends: from inside
/// it, github.com and other hosts whose TLS handshake spans several
/// packets are unreachable in silence, while reaching them from the pod
/// itself takes a fraction of a second.
///
/// The MTU is asserted relative to what the pod itself reports for the
/// interface its default route uses — whatever the cluster's overlay
/// happens to have made it, which is exactly what `start-dockerd.sh` is
/// meant to have taken, and is checked against dockerd's own arguments.
#[sqlx::test]
async fn test_docker_containers_get_the_pods_uplink_mtu(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    // Longer than `SCENARIO_LIMIT`: importing a base image from the
    // sandbox's own filesystem through exec streams its whole root
    // filesystem, as SME-33's docker test found.
    let scenario_limit = Duration::from_secs(420);
    run_then_tear_down(&pool, &client, scenario_limit, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let instance = db::smelt_instance(&pool).await.expect("instance");
        let docker = DockerSidecar {
            memory: "1Gi".to_string(),
            storage: PodStorage::Conversation(conversation.id),
        };
        ensure_conversation_pvcs(&client, conversation.id, &instance)
            .await
            .expect("ensure conversation claims");
        let pod = get()
            .expect("this test's manager")
            .create_with_docker(
                &super::super::tests::unique_session_id("docker-mtu"),
                "256Mi",
                &docker,
                &[],
                &instance,
            )
            .await
            .expect("create a pod with a Docker sidecar");

        // The pod's own uplink MTU, read the way start-dockerd.sh reads it:
        // the sysfs MTU of the interface carrying the default route.
        let uplink = pod
            .exec(&[
                "sh",
                "-c",
                "while read -r iface dest _; do [ \"$dest\" = \"00000000\" ] && break; done < /proc/net/route; cat /sys/class/net/$iface/mtu",
            ])
            .await
            .expect("read the pod's uplink MTU");
        assert_eq!(uplink.exit_code, 0, "read uplink mtu: {}", uplink.stderr);
        let uplink_mtu: u32 = uplink
            .stdout
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("uplink mtu {:?} isn't a number", uplink.stdout));

        // What dockerd was actually told — the value of its --mtu flag and
        // of its bridge default-network-opt — read from its own cmdline in
        // the sidecar, so the check below can't pass on a coincidence.
        let flags = pod
            .exec_in(
                "docker",
                &[
                    "sh",
                    "-c",
                    "tr '\\0' '\\n' < /proc/$(cat /var/run/docker.pid)/cmdline | grep -e '^--mtu=' -e 'com.docker.network.driver.mtu='",
                ],
            )
            .await
            .expect("read dockerd's flags");
        let flag_mtus: Vec<u32> = flags
            .stdout
            .lines()
            .map(|line| {
                line.rsplit('=')
                    .next()
                    .unwrap_or_default()
                    .parse()
                    .unwrap_or_else(|_| panic!("dockerd flag line {line:?} has no MTU number"))
            })
            .collect();
        assert_eq!(
            flag_mtus.len(),
            2,
            "dockerd should carry both --mtu and --default-network-opt, got {:?}",
            flags.stdout
        );
        assert!(
            flag_mtus.iter().all(|m| *m == uplink_mtu),
            "dockerd's MTU flags {flag_mtus:?} should all be the pod's uplink MTU {uplink_mtu}"
        );

        // A base image from the sandbox's own files: no Docker Hub (SME-33).
        let import = pod
            .exec(&[
                "bash",
                "-c",
                "sudo tar -C / -c bin sbin lib lib64 usr etc 2>/dev/null | docker import - local/base",
            ])
            .await
            .expect("import base image");
        assert_eq!(import.exit_code, 0, "import base image: {}", import.stdout);

        // A container on the default bridge: what --mtu governs.
        let on_default = pod
            .exec(&["docker", "run", "--rm", "local/base", "cat", "/sys/class/net/eth0/mtu"])
            .await
            .expect("run container on default bridge");
        assert_eq!(on_default.exit_code, 0, "container run: {}", on_default.stderr);
        let bridge_mtu: u32 = on_default
            .stdout
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("default bridge container mtu {:?}", on_default.stdout));

        // A container on a user-defined bridge created now, after dockerd
        // came up: what only --default-network-opt reaches. The same choice
        // docker compose makes for a project's network, and a model's own
        // `docker network create`.
        let create_net = pod
            .exec(&["docker", "network", "create", "sme157-probe"])
            .await
            .expect("create a user-defined bridge");
        assert_eq!(create_net.exit_code, 0, "network create: {}", create_net.stderr);
        let on_user_net = pod
            .exec(&["docker", "run", "--rm", "--network", "sme157-probe", "local/base", "cat", "/sys/class/net/eth0/mtu"])
            .await
            .expect("run container on the user-defined bridge");
        assert_eq!(on_user_net.exit_code, 0, "container run: {}", on_user_net.stderr);
        let user_net_mtu: u32 = on_user_net
            .stdout
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("user bridge container mtu {:?}", on_user_net.stdout));

        assert_eq!(
            bridge_mtu, uplink_mtu,
            "a container on the default bridge got MTU {bridge_mtu}, not the pod's uplink MTU {uplink_mtu}"
        );
        assert_eq!(
            user_net_mtu, uplink_mtu,
            "a container on a user-defined bridge got MTU {user_net_mtu}, not the pod's uplink MTU {uplink_mtu} — \
             --default-network-opt is what reaches these"
        );

        std::mem::forget(pod);
    })
    .await;
}
