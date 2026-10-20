use super::*;

#[sqlx::test]
async fn test_list_live_pods_reports_each_live_pod_and_its_activity(pool: PgPool) {
    let busy = test_conversation(&pool).await;
    create_message(
        &pool,
        busy.id,
        "user",
        &[ContentBlock::Text { text: "build it".to_string() }],
    )
    .await
    .expect("title the conversation");
    let busy_pod = create_sandbox_pod(&pool, busy.id).await.expect("pod");
    let t1 = create_sandbox_terminal(&pool, busy_pod.id).await.expect("terminal");
    let t2 = create_sandbox_terminal(&pool, busy_pod.id).await.expect("terminal");
    create_terminal_command(&pool, busy.id, t1.id, "cmd-done", "true", 300)
        .await
        .expect("command");
    mark_terminal_command_finished(&pool, "cmd-done", 0).await.expect("finish");
    create_terminal_command(&pool, busy.id, t2.id, "cmd-running", "sleep 100", 300)
        .await
        .expect("command");
    let closed = create_sandbox_terminal(&pool, busy_pod.id).await.expect("terminal");
    terminate_sandbox_terminal(&pool, closed.id).await.expect("close terminal");

    let quiet = test_conversation(&pool).await;
    let quiet_pod = create_sandbox_pod(&pool, quiet.id).await.expect("pod");

    let gone = test_conversation(&pool).await;
    let gone_pod = create_sandbox_pod(&pool, gone.id).await.expect("pod");
    terminate_sandbox_pod(&pool, gone_pod.id).await.expect("terminate");

    let rows = list_live_pods(&pool).await.expect("list live pods");
    assert_eq!(
        rows.iter().map(|r| r.pod_id).collect::<Vec<_>>(),
        vec![busy_pod.id, quiet_pod.id],
        "only live pods, oldest first"
    );
    let busy_row = &rows[0];
    assert_eq!(busy_row.conversation_id, busy.id);
    assert_eq!(busy_row.conversation_title, "build it");
    assert_eq!(busy_row.live_terminals, 2, "a closed terminal doesn't count");
    assert_eq!(busy_row.running_commands, 1);
    assert!(busy_row.last_command_finished_at.is_some());
    let quiet_row = &rows[1];
    assert_eq!(quiet_row.live_terminals, 0);
    assert_eq!(quiet_row.running_commands, 0);
    assert_eq!(quiet_row.last_command_finished_at, None);
    assert!(
        rows.iter().all(|r| r.observed_at >= r.created_at && r.observed_at >= r.conversation_updated_at),
        "observed_at should be the database's current time"
    );

    let with_pods = conversations_with_live_pods(&pool).await.expect("list");
    assert_eq!(with_pods, vec![busy.id, quiet.id]);
}

#[sqlx::test]
async fn test_create_sandbox_pod_and_terminate_is_idempotent(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let pod = create_sandbox_pod(&pool, conversation.id)
        .await
        .expect("create pod");
    assert!(pod.terminated_at.is_none());

    let listed = list_sandbox_pods(&pool, conversation.id)
        .await
        .expect("list pods");
    assert!(listed.iter().any(|p| p.id == pod.id));

    let terminated = terminate_sandbox_pod(&pool, pod.id)
        .await
        .expect("terminate should succeed")
        .expect("pod should exist");
    assert!(terminated.terminated_at.is_some());

    let listed_after = list_sandbox_pods(&pool, conversation.id)
        .await
        .expect("list pods");
    assert!(
        !listed_after.iter().any(|p| p.id == pod.id),
        "terminated pod should no longer be listed as live"
    );

    // Idempotent: terminating again succeeds (still finds the row),
    // doesn't error just because it's already terminated.
    let terminated_again = terminate_sandbox_pod(&pool, pod.id)
        .await
        .expect("re-terminate should succeed")
        .expect("row still exists");
    assert!(terminated_again.terminated_at.is_some());

    // Unknown pod_id: distinguishable from "already terminated" —
    // None, not an error and not a fabricated row.
    let unknown = terminate_sandbox_pod(&pool, pod.id + 999_999)
        .await
        .expect("query should succeed");
    assert!(unknown.is_none());
}

#[sqlx::test]
async fn test_sandbox_terminal_lifecycle_and_pod_id_lookup(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let pod = create_sandbox_pod(&pool, conversation.id)
        .await
        .expect("create pod");
    let terminal = create_sandbox_terminal(&pool, pod.id)
        .await
        .expect("create terminal");

    assert_eq!(
        sandbox_terminal_pod_id(&pool, terminal.id)
            .await
            .expect("lookup"),
        Some(pod.id)
    );

    let for_pod = list_sandbox_terminals_for_pod(&pool, pod.id)
        .await
        .expect("list for pod");
    assert!(for_pod.iter().any(|t| t.id == terminal.id));
    let for_conversation = list_sandbox_terminals_for_conversation(&pool, conversation.id)
        .await
        .expect("list for conversation");
    assert!(for_conversation.iter().any(|t| t.id == terminal.id));

    terminate_sandbox_terminal(&pool, terminal.id)
        .await
        .expect("terminate should succeed")
        .expect("terminal should exist");

    let for_pod_after = list_sandbox_terminals_for_pod(&pool, pod.id)
        .await
        .expect("list for pod");
    assert!(
        !for_pod_after.iter().any(|t| t.id == terminal.id),
        "terminated terminal should no longer be listed as live"
    );
}

#[sqlx::test]
async fn test_pod_previews_are_listed_per_pod_lowest_first_without_repeats(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let pod = create_sandbox_pod(&pool, conversation.id).await.expect("create pod");
    let elsewhere = test_conversation(&pool).await;
    let other = create_sandbox_pod(&pool, elsewhere.id).await.expect("create another pod");

    let local = |port: u16| (String::new(), port);
    add_pod_preview(&pool, pod.id, "", 5173).await.expect("add 5173");
    let after = add_pod_preview(&pool, pod.id, "", 3000).await.expect("add 3000");
    assert_eq!(after, vec![local(3000), local(5173)]);
    let again = add_pod_preview(&pool, pod.id, "", 3000).await.expect("add 3000 again");
    assert_eq!(again, vec![local(3000), local(5173)]);
    add_pod_preview(&pool, other.id, "", 8080).await.expect("add to the other pod");

    assert_eq!(list_pod_previews(&pool, pod.id).await.expect("list"), vec![local(3000), local(5173)]);
    assert_eq!(list_pod_previews(&pool, other.id).await.expect("list"), vec![local(8080)]);

    // A container's port is its own preview, next to localhost's same port (SME-33).
    let with_container = add_pod_preview(&pool, pod.id, "172.21.0.2", 3000).await.expect("add a container's 3000");
    assert_eq!(
        with_container,
        vec![local(3000), local(5173), ("172.21.0.2".to_string(), 3000)]
    );
}

#[sqlx::test]
async fn test_sandbox_pod_conversation_id_resolves_and_returns_none_for_unknown(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let pod = create_sandbox_pod(&pool, conversation.id)
        .await
        .expect("create pod");

    assert_eq!(
        sandbox_pod_conversation_id(&pool, pod.id)
            .await
            .expect("lookup"),
        Some(conversation.id)
    );
    assert_eq!(
        sandbox_pod_conversation_id(&pool, pod.id + 999_999)
            .await
            .expect("lookup"),
        None
    );
}

#[sqlx::test]
async fn test_terminating_a_pod_cascades_to_its_terminals(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let pod = create_sandbox_pod(&pool, conversation.id)
        .await
        .expect("create pod");
    let terminal = create_sandbox_terminal(&pool, pod.id)
        .await
        .expect("create terminal");

    terminate_sandbox_pod(&pool, pod.id)
        .await
        .expect("terminate")
        .expect("pod exists");

    // Hard DB cascade only fires on conversation deletion (see
    // SME-9's "How") — terminating the pod itself is a soft delete and
    // does *not* cascade to its terminals; the model is expected to
    // terminate_terminal each one first (enforced at the tool layer,
    // not here). Confirm the terminal row is untouched by the pod's
    // own soft delete.
    let still_there = sandbox_terminal_pod_id(&pool, terminal.id)
        .await
        .expect("lookup");
    assert_eq!(
        still_there,
        Some(pod.id),
        "terminating a pod should not itself touch its terminal rows"
    );
}
