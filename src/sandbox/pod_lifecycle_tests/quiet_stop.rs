//! SME-78: a quiet stop against a real pod keeps what makes stopping safe,
//! the conversation's `/workspace` claim and Docker's data claim, so the
//! next `create_pod` finds the files and the images as they were.

use super::*;

/// The plan's real-pod test, with the review's assertions: a file in
/// `/workspace` and an image in the Docker sidecar, a real idle pass with
/// the real cluster delete, then `create_pod` for the same conversation.
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

        // Idle for two hours, then one real pass with the real delete.
        sqlx::query("UPDATE sandbox_pods SET created_at = created_at - interval '2 hours' WHERE id = $1")
            .bind(pod)
            .execute(&pool)
            .await
            .expect("age the pod");
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
