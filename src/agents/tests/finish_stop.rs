//! SME-78: a finished agent's pod stops at once, at the end of the turn
//! that called `finish`, and its races with a `message_agent` waking it.

use super::*;

const FINISH_NOTICE: &str = "was stopped because you finished";

/// The child answers its first call with `finish`, and later ones with
/// text; the root answers with text. The child's first call waits until
/// `ready` is notified, so the test can give it a pod first.
fn child_finishes_when(ready: Arc<tokio::sync::Notify>) -> impl Fn(&serde_json::Value) -> Reply + Send + Sync {
    let seen = Mutex::new(std::collections::HashSet::new());
    move |request| match agent_of(request) {
        Some(id) if seen.lock().expect("seen").insert(id) => {
            let ready = ready.clone();
            Box::pin(async move {
                ready.notified().await;
                tool_call_reply("finish", serde_json::json!({"report": "Done."}))
            })
        }
        Some(_) => Box::pin(async { text_reply("Reading it.") }),
        None => Box::pin(async { text_reply("Thanks.") }),
    }
}

/// A child of a fresh root whose first turn is held before its `finish`,
/// with a live pod row; returns (root, child, pod, the release).
async fn finishing_child_with_pod(pool: &PgPool) -> (i64, i64, i64, Arc<tokio::sync::Notify>) {
    db::test_support::start_ids_clear_of_other_runs(pool).await.expect("ids clear of other runs");
    let ready = Arc::new(tokio::sync::Notify::new());
    mock_with(pool, child_finishes_when(ready.clone())).await;
    let root = db::create_conversation(pool).await.expect("create conversation").id;
    let child = start_child(pool, root, "Write the file, then finish.").await;
    // Only once its start has ended: a start that finds a pod writes the
    // child's credentials into it, which needs a cluster.
    eventually("the child's start ends", || async { test_hooks::starts_ended(child) }).await;
    assert_eq!(state(pool, child).await.as_deref(), Some(db::AGENT_ACTIVE), "the start succeeded");
    let pod = db::create_sandbox_pod(pool, child).await.expect("the child's pod").id;
    (root, child, pod, ready)
}

async fn live(pool: &PgPool, pod: i64) -> bool {
    db::sandbox_pod_is_live(pool, pod).await.expect("live?")
}

async fn finish_notices(pool: &PgPool, child: i64) -> usize {
    transcript(pool, child).await.iter().filter(|(_, text)| text.contains(FINISH_NOTICE)).count()
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_childs_finish_stops_its_pod_and_leaves_its_volumes(pool: PgPool) {
    let (root, child, pod, ready) = finishing_child_with_pod(&pool).await;
    // A volume the root made and passed to the child, which mounts it
    // (review 1, A5): the child's own mount row is the one a stop could touch.
    let mut conn = pool.acquire().await.expect("a connection");
    let volume = db::create_agent_volume_with_owner_mount(&mut conn, root, "shared", "/mnt/shared").await.expect("a volume");
    drop(conn);
    sqlx::query("INSERT INTO conversation_volume_mounts (conversation_id, volume_id) VALUES ($1, $2)")
        .bind(child)
        .bind(volume.id)
        .execute(&pool)
        .await
        .expect("the child mounts it");
    crate::sandbox::fake_delete::succeed(pod);

    ready.notify_one();
    eventually("the finish stops the pod", || async { !live(&pool, pod).await }).await;

    assert_eq!(state(&pool, child).await.as_deref(), Some(db::AGENT_FINISHED));
    eventually("the notice is saved", || async { finish_notices(&pool, child).await == 1 }).await;
    let volume_kept: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM agent_volumes WHERE id = $1)")
        .bind(volume.id)
        .fetch_one(&pool)
        .await
        .expect("read");
    let mounts: Vec<i64> = sqlx::query_scalar("SELECT conversation_id FROM conversation_volume_mounts WHERE volume_id = $1 ORDER BY conversation_id")
        .bind(volume.id)
        .fetch_all(&pool)
        .await
        .expect("read");
    assert!(volume_kept, "the stop deleted the volume");
    assert_eq!(mounts, vec![root, child], "the stop touches neither the owner's nor the child's mount row");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_child_that_finishes_with_a_command_running_keeps_its_pod_until_it_ends(pool: PgPool) {
    let (_, child, pod, ready) = finishing_child_with_pod(&pool).await;
    let terminal = db::create_sandbox_terminal(&pool, pod).await.expect("terminal");
    let command = format!("left-up-{child}");
    db::create_terminal_command(&pool, child, terminal.id, &command, "python3 -m http.server", 0)
        .await
        .expect("a command left running");
    crate::sandbox::fake_delete::succeed(pod);

    ready.notify_one();
    eventually("the child finished", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_FINISHED) }).await;
    eventually("after_last_turn ran", || async { test_hooks::last_turns_checked(child) >= 1 }).await;
    assert!(live(&pool, pod).await, "a running command keeps the pod (the user's answer 1)");
    assert_eq!(finish_notices(&pool, child).await, 0);

    db::mark_terminal_command_finished(&pool, &command, 0).await.expect("it ends");
    let stopped = crate::sandbox::stop_idle_pods_once_with(&pool, 60, &crate::sandbox::ActivityNotes::default(), |_| async {
        Ok::<(), crate::sandbox::SandboxError>(())
    })
    .await;
    assert_eq!(stopped, vec![pod], "the next pass's backstop takes it");
    assert_eq!(finish_notices(&pool, child).await, 1);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_child_with_no_pod_finishes_with_no_stop_and_no_notice(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let ready = Arc::new(tokio::sync::Notify::new());
    mock_with(&pool, child_finishes_when(ready.clone())).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = start_child(&pool, root, "Finish.").await;

    ready.notify_one();
    eventually("the child finished", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_FINISHED) }).await;
    eventually("after_last_turn ran", || async { test_hooks::last_turns_checked(child) >= 1 }).await;
    assert_eq!(finish_notices(&pool, child).await, 0);
    assert!(!crate::sandbox::stop_finished_agent_pod_with(&pool, child, |_| async { Ok(()) }).await, "nothing to stop");
}

/// The stop wins: it holds the turn lock and has re-checked when the
/// message lands, so the woken turn runs after it with no pod, reading the
/// notice and then the message.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_message_landing_after_the_stops_recheck_finds_the_pod_stopped(pool: PgPool) {
    let (root, child, pod, ready) = finishing_child_with_pod(&pool).await;
    let (reached, release) = crate::sandbox::fake_delete::hold(pod);

    ready.notify_one();
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the stop reaches its delete").expect("hook");
    let sent = crate::anthropic::tools::execute(&pool, root, "toolu_m", "message_agent", &serde_json::json!({"to": child, "text": "cat /workspace/child.txt"}))
        .await
        .expect("message_agent");
    assert!(!sent.contains("rror"), "{sent}");
    let _ = release.send(());

    eventually("the woken turn answers", || async {
        transcript(&pool, child).await.iter().any(|(role, text)| role == "assistant" && text == "Reading it.")
    })
    .await;
    assert!(!live(&pool, pod).await, "the stop won");
    let texts: Vec<String> = transcript(&pool, child).await.into_iter().map(|(_, text)| text).collect();
    let notice = texts.iter().position(|t| t.contains(FINISH_NOTICE)).expect("the finish notice");
    let message = texts.iter().position(|t| t.contains("cat /workspace/child.txt")).expect("the message");
    assert!(notice < message, "the notice, then the message: {texts:?}");
}

/// The wake wins: the message set the child `active` before the stop's
/// re-check, so the stop skips and the pod stays.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_message_landing_before_the_stops_recheck_keeps_the_pod(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    mock_upstream(&pool, vec![text_reply("On it.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = active_child(&pool, root).await;
    let pod = db::create_sandbox_pod(&pool, child).await.expect("pod").id;
    sqlx::query("UPDATE agents SET state = 'finished' WHERE conversation_id = $1")
        .bind(child)
        .execute(&pool)
        .await
        .expect("finished");
    let outcome = db::message_agent(&pool, root, db::Recipient::Child(child), "parent", "one more thing")
        .await
        .expect("message_agent");
    let _ = outcome;
    assert_eq!(state(&pool, child).await.as_deref(), Some("active"), "the message woke it");

    let stopped = crate::sandbox::stop_finished_agent_pod_with(&pool, child, |_| async { Ok(()) }).await;

    assert!(!stopped, "the re-check read active and skipped");
    assert!(live(&pool, pod).await, "the woken child keeps its pod");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_child_stop_agent_stopped_keeps_its_pod(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = active_child(&pool, root).await;
    let pod = db::create_sandbox_pod(&pool, child).await.expect("pod").id;
    sqlx::query("UPDATE agents SET state = 'stopped' WHERE conversation_id = $1")
        .bind(child)
        .execute(&pool)
        .await
        .expect("stopped");
    crate::sandbox::fake_delete::succeed(pod);

    after_last_turn(&pool, child, None).await;

    assert!(live(&pool, pod).await, "stop leaves the sandbox as it is");
    let stopped = crate::sandbox::stop_idle_pods_once_with(&pool, 60, &crate::sandbox::ActivityNotes::default(), |_| async {
        Ok::<(), crate::sandbox::SandboxError>(())
    })
    .await;
    assert_eq!(stopped, Vec::<i64>::new(), "not a finished agent's, and not idle yet");
}

/// The finish stop waits for a turn lock someone holds (a notice being
/// saved between turns, say) rather than giving up on the pod: a
/// `try_lock` would leave it for the backstop.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_finish_stop_waits_for_a_held_turn_lock(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = active_child(&pool, root).await;
    let pod = db::create_sandbox_pod(&pool, child).await.expect("pod").id;
    sqlx::query("UPDATE agents SET state = 'finished' WHERE conversation_id = $1")
        .bind(child)
        .execute(&pool)
        .await
        .expect("finished");
    let lock = crate::turn::conversation_lock(child);
    let held = lock.clone().lock_owned().await;

    let stop = tokio::spawn({
        let pool = pool.clone();
        async move { crate::sandbox::stop_finished_agent_pod_with(&pool, child, |_| async { Ok(()) }).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!stop.is_finished(), "the stop waits for the lock");
    assert!(live(&pool, pod).await);
    drop(held);
    drop(lock);

    let stopped = tokio::time::timeout(Duration::from_secs(10), stop).await.expect("the stop ends").expect("join");
    assert!(stopped, "stopped once the lock was free");
    assert!(!live(&pool, pod).await);
}
