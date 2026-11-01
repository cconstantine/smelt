//! SME-78: a quiet stop against a real pod keeps what makes stopping safe,
//! the conversation's `/workspace` claim and Docker's data claim, so the
//! next `create_pod` finds the files and the images as they were.

use super::*;

/// The plan's real-pod test, with the reviews' assertions: a file in
/// `/workspace` and an image in the Docker sidecar; a real `sleep 600`
/// keeping the pod through a pass, then interrupted with `send_signal`; a
/// real idle pass with the real cluster delete; then `create_pod` for the
/// same conversation.
/// The image is imported from the pod's own files, as the Docker test's
/// is, so nothing is pulled from a registry.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_quiet_stop_keeps_the_workspace_and_dockers_images(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    run_then_tear_down(&pool, &client, SCENARIO_LIMIT, async {
        let conversation = db::create_conversation(&pool).await.expect("create conversation");
        let pod = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("create_pod");
        let made = exec_with(
            &client,
            &pod_name(pod),
            "sandbox",
            &[
                "sh",
                "-c",
                "echo kept-78 > /workspace/kept.txt \
                 && sudo tar -C / -c bin sbin lib lib64 usr etc 2>/dev/null | docker import - local/kept-78 >/dev/null \
                 && docker image ls -q local/kept-78",
            ],
            None,
        )
        .await
        .expect("exec");
        assert!(made.exit_code == 0 && !made.stdout.trim().is_empty(), "the file and the image: {}{}", made.stdout, made.stderr);

        // A real command, running in the real agent: the pod is busy however
        // long it's been idle (the user's answer 1; review 2, R2-5).
        let terminal = create_terminal(&pool, conversation.id).await.expect("create_terminal");
        let sleeper = format!("sleep-{}", conversation.id);
        db::create_terminal_command(&pool, conversation.id, terminal, &sleeper, "sleep 600", 0)
            .await
            .expect("create_terminal_command");
        send_command(&pool, terminal, &sleeper, "sleep 600").await.expect("send_command");
        let back_two_hours = |pod: i64| {
            let pool = pool.clone();
            async move {
                for statement in [
                    "UPDATE sandbox_pods SET created_at = created_at - interval '2 hours' WHERE id = $1",
                    "UPDATE terminal_commands tc SET created_at = tc.created_at - interval '2 hours', \
                            finished_at = tc.finished_at - interval '2 hours' \
                       FROM sandbox_terminals t WHERE t.id = tc.terminal_id AND t.pod_id = $1",
                ] {
                    sqlx::query(statement).bind(pod).execute(&pool).await.expect("move the clock back");
                }
            }
        };
        back_two_hours(pod).await;
        let kept = stop_idle_pods_once_with(&pool, 60, &ActivityNotes::default(), |id| cluster_delete(&pool, id)).await;
        assert_eq!(kept, Vec::<i64>::new(), "a running `sleep 600` keeps the pod");
        assert!(db::sandbox_pod_is_live(&pool, pod).await.expect("live?"));

        // Interrupted, it ends; two hours on, one real pass with the real
        // delete stops the pod.
        send_signal(&pool, terminal, &sleeper, "INT").await.expect("send_signal");
        let ended = poll_until_finished(&pool, &sleeper).await;
        assert_ne!(ended.status, "running", "{ended:?}");
        terminate_terminal(&pool, terminal).await.expect("terminate_terminal");
        back_two_hours(pod).await;
        let stopped = stop_idle_pods_once_with(&pool, 60, &ActivityNotes::default(), |id| cluster_delete(&pool, id)).await;
        assert_eq!(stopped, vec![pod], "the pass stopped the pod");
        assert!(!db::sandbox_pod_is_live(&pool, pod).await.expect("live?"), "its row is closed");
        let notices: Vec<String> = db::list_messages(&pool, conversation.id)
            .await
            .expect("list messages")
            .into_iter()
            .filter(|m| m.content.contains("was stopped after 60 minutes"))
            .map(|m| m.content)
            .collect();
        assert_eq!(notices.len(), 1, "one notice: {notices:?}");

        let next = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await.expect("a new pod");
        let kept = exec_with(
            &client,
            &pod_name(next),
            "sandbox",
            &["sh", "-c", "cat /workspace/kept.txt && docker image ls -q local/kept-78"],
            None,
        )
        .await
        .expect("exec");
        let mut lines = kept.stdout.lines();
        assert_eq!(lines.next(), Some("kept-78"), "/workspace survives the stop: {}{}", kept.stdout, kept.stderr);
        assert!(
            lines.next().is_some_and(|id| !id.trim().is_empty()),
            "the image survives in Docker's data: {}{}",
            kept.stdout,
            kept.stderr
        );
        terminate_pod(&pool, conversation.id).await.expect("terminate the new pod");
    })
    .await;
}
