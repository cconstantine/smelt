use super::*;

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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
    assert!(busy_row.busy, "a running command makes the pod busy");
    let quiet_row = &rows[1];
    assert_eq!(quiet_row.live_terminals, 0);
    assert_eq!(quiet_row.running_commands, 0);
    assert!(!quiet_row.busy);
    assert_eq!(quiet_row.idle_since, quiet_row.created_at, "nothing happened since it started");
    assert!(
        rows.iter().all(|r| r.observed_at >= r.created_at && r.observed_at >= r.idle_since),
        "observed_at should be the database's current time"
    );

    let with_pods = conversations_with_live_pods(&pool).await.expect("list");
    assert_eq!(with_pods, vec![busy.id, quiet.id]);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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

// --- Quiet stops (SME-78) ---

/// Moves every timestamp of `pod_id`, its terminals' commands and the pod
/// itself back by `minutes`, so the pod reads as idle that long.
async fn age_pod(pool: &PgPool, pod_id: i64, minutes: i64) {
    let ago = format!("{minutes} minutes");
    sqlx::query(
        "UPDATE sandbox_pods SET created_at = created_at - $2::interval,
                last_active_at = last_active_at - $2::interval WHERE id = $1",
    )
    .bind(pod_id)
    .bind(&ago)
    .execute(pool)
    .await
    .expect("age the pod");
    sqlx::query(
        "UPDATE terminal_commands tc SET created_at = tc.created_at - $2::interval,
                finished_at = tc.finished_at - $2::interval
           FROM sandbox_terminals t WHERE t.id = tc.terminal_id AND t.pod_id = $1",
    )
    .bind(pod_id)
    .bind(&ago)
    .execute(pool)
    .await
    .expect("age its commands");
}

async fn idle_since(pool: &PgPool, pod_id: i64) -> NaiveDateTime {
    list_live_pods(pool)
        .await
        .expect("list live pods")
        .into_iter()
        .find(|r| r.pod_id == pod_id)
        .expect("the pod is live")
        .idle_since
}

async fn busy(pool: &PgPool, pod_id: i64) -> bool {
    list_live_pods(pool)
        .await
        .expect("list live pods")
        .into_iter()
        .find(|r| r.pod_id == pod_id)
        .expect("the pod is live")
        .busy
}

/// A child conversation with an agent row in `state`, under a fresh parent.
async fn agent_conversation(pool: &PgPool, state: &str) -> i64 {
    let parent = test_conversation(pool).await;
    let child = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ('child', $1) RETURNING id")
        .bind(parent.id)
        .fetch_one(pool)
        .await
        .expect("a child row");
    sqlx::query(
        "INSERT INTO agents (conversation_id, root_id, depth, brief, state) VALUES ($1, $2, 1, 'a brief', $3)",
    )
    .bind(child)
    .bind(parent.id)
    .bind(state)
    .execute(pool)
    .await
    .expect("its agent row");
    child
}

async fn set_agent_state(pool: &PgPool, conversation_id: i64, state: &str) {
    sqlx::query("UPDATE agents SET state = $2 WHERE conversation_id = $1")
        .bind(conversation_id)
        .bind(state)
        .execute(pool)
        .await
        .expect("set the agent's state");
}

async fn candidates(pool: &PgPool, minutes: u32) -> Vec<(i64, QuietStopKind)> {
    quiet_stop_candidates(pool, minutes)
        .await
        .expect("candidates")
        .into_iter()
        .map(|(pod, _, kind)| (pod, kind))
        .collect()
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_idle_since_moves_with_each_counted_signal(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let pod = create_sandbox_pod(&pool, conversation.id).await.expect("pod");
    let terminal = create_sandbox_terminal(&pool, pod.id).await.expect("terminal");
    age_pod(&pool, pod.id, 120).await;
    let started = idle_since(&pool, pod.id).await;
    assert_eq!(started, pod.created_at - chrono::Duration::minutes(120), "the pod's start");

    flush_pod_activity(&pool, &[conversation.id]).await.expect("flush");
    let flushed = idle_since(&pool, pod.id).await;
    assert!(flushed > started + chrono::Duration::minutes(100), "a flushed note: {flushed} after {started}");

    age_pod(&pool, pod.id, 120).await;
    let aged = idle_since(&pool, pod.id).await;
    create_terminal_command(&pool, conversation.id, terminal.id, "cmd-start", "true", 300)
        .await
        .expect("command");
    let command_started = idle_since(&pool, pod.id).await;
    assert!(command_started > aged + chrono::Duration::minutes(100), "a command's start");

    age_pod(&pool, pod.id, 120).await;
    let aged = idle_since(&pool, pod.id).await;
    mark_terminal_command_finished(&pool, "cmd-start", 0).await.expect("finish");
    let finished = idle_since(&pool, pod.id).await;
    assert!(finished > aged + chrono::Duration::minutes(100), "a command's finish");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_new_message_does_not_move_idle_since(pool: PgPool) {
    // SME-79's idle notices are messages, every 5 minutes: they mustn't
    // keep a pod alive (the user's answer 2).
    let conversation = test_conversation(&pool).await;
    let pod = create_sandbox_pod(&pool, conversation.id).await.expect("pod");
    age_pod(&pool, pod.id, 120).await;
    let before = idle_since(&pool, pod.id).await;
    create_message(&pool, conversation.id, "user", &[ContentBlock::Text { text: "a notice".to_string() }])
        .await
        .expect("a message");
    assert_eq!(idle_since(&pool, pod.id).await, before);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_cloning_repo_or_any_running_command_makes_the_pod_busy(pool: PgPool) {
    let cloning = test_conversation(&pool).await;
    let cloning_pod = create_sandbox_pod(&pool, cloning.id).await.expect("pod");
    let repo = create_conversation_repo(&pool, cloning.id, "https://example.com/r.git", "example.com/r", None, "r")
        .await
        .expect("a cloning repo");
    assert!(busy(&pool, cloning_pod.id).await, "a repo cloning");
    sqlx::query("UPDATE conversation_repos SET status = 'ready' WHERE id = $1")
        .bind(repo.id)
        .execute(&pool)
        .await
        .expect("cloned");
    assert!(!busy(&pool, cloning_pod.id).await, "a cloned repo");

    for notice_secs in [0, 300] {
        let conversation = test_conversation(&pool).await;
        let terminal = test_terminal(&pool, conversation.id).await;
        let pod = sandbox_terminal_pod_id(&pool, terminal).await.expect("lookup").expect("its pod");
        let id = format!("cmd-{notice_secs}-{}", conversation.id);
        create_terminal_command(&pool, conversation.id, terminal, &id, "npm run dev", notice_secs)
            .await
            .expect("command");
        assert!(busy(&pool, pod).await, "a running command, idle_notice_secs {notice_secs}");
    }
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_candidates_are_idle_past_the_threshold_or_a_finished_agents_never_busy(pool: PgPool) {
    let idle = test_conversation(&pool).await;
    let idle_pod = create_sandbox_pod(&pool, idle.id).await.expect("pod");
    age_pod(&pool, idle_pod.id, 61).await;

    let young = test_conversation(&pool).await;
    let young_pod = create_sandbox_pod(&pool, young.id).await.expect("pod");
    age_pod(&pool, young_pod.id, 59).await;

    let running = test_conversation(&pool).await;
    let terminal = test_terminal(&pool, running.id).await;
    let running_pod = sandbox_terminal_pod_id(&pool, terminal).await.expect("lookup").expect("pod");
    create_terminal_command(&pool, running.id, terminal, "cmd-held", "sleep 600", 0)
        .await
        .expect("command");
    age_pod(&pool, running_pod, 600).await;

    let gone = test_conversation(&pool).await;
    let gone_pod = create_sandbox_pod(&pool, gone.id).await.expect("pod");
    age_pod(&pool, gone_pod.id, 600).await;
    terminate_sandbox_pod(&pool, gone_pod.id).await.expect("terminate");

    let finished = agent_conversation(&pool, "finished").await;
    let finished_pod = create_sandbox_pod(&pool, finished).await.expect("pod");

    let active = agent_conversation(&pool, "active").await;
    let active_pod = create_sandbox_pod(&pool, active).await.expect("pod");

    assert_eq!(
        candidates(&pool, 60).await,
        vec![(idle_pod.id, QuietStopKind::Idle), (finished_pod.id, QuietStopKind::Finished)],
        "idle past 60, or a finished agent's at any age; never busy, young, terminated or an active agent's"
    );
    assert_eq!(
        candidates(&pool, 0).await,
        vec![(finished_pod.id, QuietStopKind::Finished)],
        "at 0 no pod is idle, and a finished agent's still is a candidate"
    );
    let _ = active_pod;
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_finished_agents_pod_with_a_running_command_is_a_candidate_once_it_ends(pool: PgPool) {
    let finished = agent_conversation(&pool, "finished").await;
    let terminal = test_terminal(&pool, finished).await;
    let pod = sandbox_terminal_pod_id(&pool, terminal).await.expect("lookup").expect("pod");
    create_terminal_command(&pool, finished, terminal, "cmd-left-up", "python3 -m http.server", 300)
        .await
        .expect("command");
    assert_eq!(candidates(&pool, 60).await, vec![], "a running command keeps it (the user's answer 1)");
    mark_terminal_command_finished(&pool, "cmd-left-up", 130).await.expect("finish");
    assert_eq!(candidates(&pool, 60).await, vec![(pod, QuietStopKind::Finished)]);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_pod_still_stoppable_turns_false_on_each_change(pool: PgPool) {
    // Idle: fresh activity, a clone starting, a command starting, a terminate.
    async fn idle_pod(pool: &PgPool) -> (i64, i64) {
        let conversation = test_conversation(pool).await;
        let pod = create_sandbox_pod(pool, conversation.id).await.expect("pod");
        age_pod(pool, pod.id, 90).await;
        assert!(
            pod_still_stoppable(pool, pod.id, QuietStopKind::Idle, 60).await.expect("re-check"),
            "an idle pod is stoppable"
        );
        (conversation.id, pod.id)
    }
    let (conversation, pod) = idle_pod(&pool).await;
    flush_pod_activity(&pool, &[conversation]).await.expect("flush");
    assert!(!pod_still_stoppable(&pool, pod, QuietStopKind::Idle, 60).await.expect("re-check"), "fresh activity");

    let (conversation, pod) = idle_pod(&pool).await;
    create_conversation_repo(&pool, conversation, "https://example.com/r.git", "example.com/r", None, "r")
        .await
        .expect("a clone");
    assert!(!pod_still_stoppable(&pool, pod, QuietStopKind::Idle, 60).await.expect("re-check"), "a clone starting");

    let (conversation, pod) = idle_pod(&pool).await;
    let terminal = create_sandbox_terminal(&pool, pod).await.expect("terminal");
    let id = format!("cmd-recheck-{conversation}");
    create_terminal_command(&pool, conversation, terminal.id, &id, "make", 300).await.expect("command");
    sqlx::query("UPDATE terminal_commands SET created_at = created_at - interval '90 minutes' WHERE command_id = $1")
        .bind(&id)
        .execute(&pool)
        .await
        .expect("age the command");
    assert!(
        !pod_still_stoppable(&pool, pod, QuietStopKind::Idle, 60).await.expect("re-check"),
        "a command running, however old"
    );

    let (_, pod) = idle_pod(&pool).await;
    terminate_sandbox_pod(&pool, pod).await.expect("terminate");
    assert!(!pod_still_stoppable(&pool, pod, QuietStopKind::Idle, 60).await.expect("re-check"), "terminated");

    let (_, pod) = idle_pod(&pool).await;
    assert!(!pod_still_stoppable(&pool, pod, QuietStopKind::Idle, 0).await.expect("re-check"), "idle stops off");

    // Finished: the agent going active again.
    let child = agent_conversation(&pool, "finished").await;
    let child_pod = create_sandbox_pod(&pool, child).await.expect("pod");
    assert!(pod_still_stoppable(&pool, child_pod.id, QuietStopKind::Finished, 0).await.expect("re-check"));
    assert!(
        !pod_still_stoppable(&pool, child_pod.id, QuietStopKind::Idle, 60).await.expect("re-check"),
        "a fresh finished agent's pod isn't idle"
    );
    set_agent_state(&pool, child, "active").await;
    assert!(
        !pod_still_stoppable(&pool, child_pod.id, QuietStopKind::Finished, 0).await.expect("re-check"),
        "the agent was woken"
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_flush_pod_activity_writes_only_live_pods(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let old = create_sandbox_pod(&pool, conversation.id).await.expect("pod");
    terminate_sandbox_pod(&pool, old.id).await.expect("terminate");
    let live = create_sandbox_pod(&pool, conversation.id).await.expect("pod");
    let other = test_conversation(&pool).await;
    let untouched = create_sandbox_pod(&pool, other.id).await.expect("pod");

    flush_pod_activity(&pool, &[conversation.id]).await.expect("flush");

    let stamps: Vec<(i64, Option<NaiveDateTime>)> =
        sqlx::query_as("SELECT id, last_active_at FROM sandbox_pods WHERE id = ANY($1) ORDER BY id")
            .bind(vec![old.id, live.id, untouched.id])
            .fetch_all(&pool)
            .await
            .expect("read stamps");
    assert_eq!(stamps[0], (old.id, None), "a terminated pod");
    assert_eq!(stamps[1].0, live.id);
    assert!(stamps[1].1.is_some(), "the live pod is stamped");
    assert_eq!(stamps[2], (untouched.id, None), "another conversation's pod");
}
