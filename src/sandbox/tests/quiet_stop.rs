//! SME-78's quiet stops, against the database with the cluster's delete
//! faked: the idle pass, its skips and re-check, and the stop itself.

use super::*;

/// The cluster's delete, faked to succeed.
async fn deleted(_pod_id: i64) -> Result<(), SandboxError> {
    Ok(())
}

/// The cluster's delete, faked to fail.
async fn delete_fails(_pod_id: i64) -> Result<(), SandboxError> {
    Err(SandboxError::StartFailed("the API server is down".into()))
}

/// A conversation with a live pod idle for `minutes`, and the pod's id.
async fn idle_pod(pool: &PgPool, minutes: i64) -> (i64, i64) {
    let conversation = db::create_conversation(pool).await.expect("conversation");
    let pod = db::create_sandbox_pod(pool, conversation.id).await.expect("pod");
    age(pool, pod.id, minutes).await;
    (conversation.id, pod.id)
}

/// Moves the pod's start, its note and its commands' times back `minutes`.
async fn age(pool: &PgPool, pod_id: i64, minutes: i64) {
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

/// An agent's conversation, under a fresh parent: `active` until
/// `mark_finished`, which a test calls once it has given it a pod.
async fn finished_agent(pool: &PgPool) -> i64 {
    let parent = db::create_conversation(pool).await.expect("parent");
    let child = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ('child', $1) RETURNING id")
        .bind(parent.id)
        .fetch_one(pool)
        .await
        .expect("a child row");
    sqlx::query("INSERT INTO agents (conversation_id, root_id, depth, brief, state) VALUES ($1, $2, 1, 'b', 'active')")
        .bind(child)
        .bind(parent.id)
        .execute(pool)
        .await
        .expect("its agent row");
    child
}

/// Marks the agent finished now, after its pod, as a real finish comes.
async fn mark_finished(pool: &PgPool, conversation_id: i64) {
    sqlx::query("UPDATE agents SET state = 'finished', state_changed_at = now() WHERE conversation_id = $1")
        .bind(conversation_id)
        .execute(pool)
        .await
        .expect("finish the agent");
}

/// The texts of `conversation_id`'s messages that contain `needle`.
async fn notices_with(pool: &PgPool, conversation_id: i64, needle: &str) -> Vec<String> {
    db::list_messages(pool, conversation_id)
        .await
        .expect("list messages")
        .into_iter()
        .filter(|m| m.content.contains(needle))
        .map(|m| m.content)
        .collect()
}

async fn updated_at(pool: &PgPool, conversation_id: i64) -> chrono::NaiveDateTime {
    sqlx::query_scalar("SELECT updated_at FROM conversations WHERE id = $1")
        .bind(conversation_id)
        .fetch_one(pool)
        .await
        .expect("updated_at")
}

async fn live(pool: &PgPool, pod_id: i64) -> bool {
    db::sandbox_pod_is_live(pool, pod_id).await.expect("live?")
}

async fn pass(pool: &PgPool, minutes: u32) -> Vec<i64> {
    stop_idle_pods_once_with(pool, minutes, &ActivityNotes::default(), deleted).await
}

#[test]
fn test_idle_stop_minutes_is_an_hour_by_default_and_zero_turns_it_off() {
    assert_eq!(idle_stop_minutes_from(None), 60);
    assert_eq!(idle_stop_minutes_from(Some(String::new())), 60, "an empty setting is no setting");
    assert_eq!(idle_stop_minutes_from(Some("0".to_string())), 0, "0 is off, not a fallback");
    assert_eq!(idle_stop_minutes_from(Some("3".to_string())), 3);
    assert_eq!(idle_stop_minutes_from(Some("10080".to_string())), 10_080, "a week is the ceiling");
    for bad in ["10081", "-1", "junk", "2.5", " 5 ", "99999999999999999999"] {
        assert_eq!(idle_stop_minutes_from(Some(bad.to_string())), 60, "{bad:?} falls back with a warning");
    }
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_idle_pod_with_no_running_command_is_stopped_quietly(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let (conversation, pod) = idle_pod(&pool, 0).await;
    let idle_terminal = db::create_sandbox_terminal(&pool, pod).await.expect("terminal");
    let used = db::create_sandbox_terminal(&pool, pod).await.expect("terminal");
    db::create_terminal_command(&pool, conversation, used.id, &format!("done-{pod}"), "make", 300)
        .await
        .expect("command");
    db::mark_terminal_command_finished(&pool, &format!("done-{pod}"), 0).await.expect("finish");
    age(&pool, pod, 61).await;
    let before = updated_at(&pool, conversation).await;
    let mut events = events::subscribe(conversation);
    let mut app = events::subscribe_app();

    assert_eq!(pass(&pool, 60).await, vec![pod]);

    assert!(!live(&pool, pod).await, "the pod's row is closed");
    assert!(
        db::list_sandbox_terminals_for_pod(&pool, pod).await.expect("terminals").is_empty(),
        "its idle terminals are closed"
    );
    let command = db::get_terminal_command(&pool, &format!("done-{pod}")).await.expect("read").expect("row");
    assert_eq!(command.status, "finished", "no command is marked lost");
    let notices = notices_with(&pool, conversation, "was stopped after 60 minutes with no activity").await;
    assert_eq!(notices.len(), 1, "one notice: {notices:?}");
    assert!(notices[0].contains(&format!("Sandbox pod {pod} ")), "{}", notices[0]);
    assert_eq!(updated_at(&pool, conversation).await, before, "the conversation doesn't move in the sidebar");
    let mut saw_terminated = false;
    let mut saw_notice = false;
    let mut saw_terminal_closed = false;
    while let Ok(event) = events.try_recv() {
        match event {
            events::ConversationEvent::SandboxPodUpdate { pod_id, terminated: true, .. } if pod_id == pod => saw_terminated = true,
            events::ConversationEvent::MessagesAppended { .. } => saw_notice = true,
            events::ConversationEvent::SandboxTerminalUpdate { terminal_id, terminated: true, .. } if terminal_id == idle_terminal.id => {
                saw_terminal_closed = true
            }
            _ => {}
        }
    }
    assert!(saw_terminated && saw_notice && saw_terminal_closed, "published: pod {saw_terminated}, notice {saw_notice}, terminal {saw_terminal_closed}");
    let mut saw_pods_changed = false;
    while let Ok(event) = app.try_recv() {
        saw_pods_changed |= matches!(event, events::AppEvent::PodsChanged);
    }
    assert!(saw_pods_changed, "PodsChanged");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_pod_is_skipped_while_its_turn_runs_or_a_lock_is_held_and_stopped_once_free(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let (conversation, pod) = idle_pod(&pool, 90).await;

    let turn = crate::turn::count_a_test_turn(conversation);
    assert_eq!(pass(&pool, 60).await, Vec::<i64>::new(), "a turn running");
    drop(turn);

    let lock = crate::turn::conversation_lock(conversation);
    let held = lock.lock().await;
    assert_eq!(pass(&pool, 60).await, Vec::<i64>::new(), "the turn lock held");
    drop(held);
    drop(lock);

    let start = pod_start_lock(conversation);
    let starting = start.lock().await;
    assert_eq!(pass(&pool, 60).await, Vec::<i64>::new(), "the start lock held");
    drop(starting);
    assert!(live(&pool, pod).await);

    assert_eq!(pass(&pool, 60).await, vec![pod], "stopped once free");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_activity_noted_after_the_candidates_are_read_saves_the_pod(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let (conversation, pod) = idle_pod(&pool, 90).await;
    let candidates = db::quiet_stop_candidates(&pool, 60).await.expect("candidates");
    assert_eq!(candidates, vec![(pod, conversation, db::QuietStopKind::Idle)]);

    // A preview request lands between the read and the re-check.
    let notes = ActivityNotes::default();
    notes.note(conversation);
    let stopped = stop_candidate(&pool, &notes, pod, conversation, StopReason::Idle { minutes: 60 }, deleted(pod)).await;

    assert!(!stopped, "the re-check's flush saw the preview");
    assert!(live(&pool, pod).await);
    assert!(!notes.contains(conversation), "the note was flushed");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_failed_delete_saves_no_notice_and_the_next_pass_stops_it_once(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let (conversation, pod) = idle_pod(&pool, 90).await;

    let failed = stop_idle_pods_once_with(&pool, 60, &ActivityNotes::default(), delete_fails).await;
    assert_eq!(failed, Vec::<i64>::new());
    assert!(live(&pool, pod).await, "the row stays live");
    assert_eq!(notices_with(&pool, conversation, "was stopped after").await.len(), 0, "no notice");

    assert_eq!(pass(&pool, 60).await, vec![pod]);
    assert_eq!(notices_with(&pool, conversation, "was stopped after").await.len(), 1, "exactly one");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_conversation_deleted_during_the_stop_ends_it_quietly(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let (conversation, pod) = idle_pod(&pool, 90).await;
    let delete_conversation = async {
        db::delete_conversation(&pool, conversation).await.expect("delete");
        Ok(())
    };
    let stopped = stop_locked(&pool, pod, conversation, StopReason::Idle { minutes: 60 }, delete_conversation).await;
    assert!(!stopped, "nothing left to stop");
    assert!(!db::conversation_exists(&pool, conversation).await.expect("exists?"));
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_pod_the_user_stopped_first_leaves_the_pass_nothing_to_do(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let (conversation, pod) = idle_pod(&pool, 90).await;
    terminate_pod_with(&pool, pod, deleted(pod)).await.expect("the user's stop");
    assert_eq!(pass(&pool, 60).await, Vec::<i64>::new());
    assert_eq!(notices_with(&pool, conversation, "was stopped after").await.len(), 0);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_at_zero_an_idle_pod_stays_and_a_finished_agents_pod_still_goes(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let (_, idle) = idle_pod(&pool, 6000).await;
    let child = finished_agent(&pool).await;
    let child_pod = db::create_sandbox_pod(&pool, child).await.expect("pod");
    mark_finished(&pool, child).await;

    assert_eq!(pass(&pool, 0).await, vec![child_pod.id], "the backstop, with idle stops off");
    assert!(live(&pool, idle).await, "idle stops are off");
    let notices = notices_with(&pool, child, "was stopped because you finished").await;
    assert_eq!(notices.len(), 1, "the finish notice: {notices:?}");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_running_command_keeps_its_pod_until_it_ends_and_the_threshold_passes(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let (conversation, pod) = idle_pod(&pool, 0).await;
    let terminal = db::create_sandbox_terminal(&pool, pod).await.expect("terminal");
    let command = format!("server-{pod}");
    db::create_terminal_command(&pool, conversation, terminal.id, &command, "python3 -m http.server", 0)
        .await
        .expect("command");
    age(&pool, pod, 600).await;
    for _ in 0..3 {
        assert_eq!(pass(&pool, 60).await, Vec::<i64>::new(), "a running command, idle for hours");
    }

    db::mark_terminal_command_finished(&pool, &command, 130).await.expect("interrupted");
    assert_eq!(pass(&pool, 60).await, Vec::<i64>::new(), "the clock starts again from its finish");
    age(&pool, pod, 61).await;
    assert_eq!(pass(&pool, 60).await, vec![pod], "the threshold passed from finished_at");
}

/// SME-78 review 1 (A1): a flush that fails under the locks skips the
/// candidate for this pass, rather than stopping it on a `last_active_at`
/// that misses the activity just noted. The next pass, its flush through,
/// sees the activity and keeps the pod.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_failed_flush_before_the_recheck_skips_the_pod(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let (conversation, pod) = idle_pod(&pool, 90).await;
    let notes = ActivityNotes::default();
    notes.note(conversation);
    notes.fail_next_flush();

    let stopped = stop_candidate(&pool, &notes, pod, conversation, StopReason::Idle { minutes: 60 }, deleted(pod)).await;

    assert!(!stopped, "stopped on a stale last_active_at");
    assert!(live(&pool, pod).await);
    assert!(notes.contains(conversation), "the note is kept for the next flush");
    assert_eq!(stop_idle_pods_once_with(&pool, 60, &notes, deleted).await, Vec::<i64>::new(), "the next pass flushes it");
}

/// SME-78 review 1 (A3): a cluster delete that never answers (a stalled
/// API server) is given up on after `QUIET_STOP_DELETE_TIMEOUT`, so the
/// turn lock isn't held for kube-rs's own timeouts. The pod is left live,
/// with no notice, for the next pass.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stalled_delete_times_out_and_leaves_the_pod(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let (conversation, pod) = idle_pod(&pool, 90).await;
    let stalled = std::future::pending::<Result<(), SandboxError>>();

    let stopped = tokio::time::timeout(
        QUIET_STOP_DELETE_TIMEOUT * 4,
        stop_locked(&pool, pod, conversation, StopReason::Idle { minutes: 60 }, stalled),
    )
    .await
    .expect("the stop gives up on a stalled delete");

    assert!(!stopped);
    assert!(live(&pool, pod).await, "the pod is left for the next pass");
    assert_eq!(notices_with(&pool, conversation, "was stopped after").await.len(), 0, "no notice");
}
