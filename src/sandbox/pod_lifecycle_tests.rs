//! The sandbox's lifecycle against a real cluster, one feature per test
//! (SME-94): pods, terminals, commands, file tools, repos and volumes,
//! each run by the real agent in a real pod.
//!
//! Every test starts with `own_sandbox`, which gives it ids no other test
//! or run shares and a sandbox manager of its own behind `get()`, and runs
//! its scenario through `run_then_tear_down`, which bounds it in time and
//! deletes what its database made in the cluster, pass or fail. So the
//! tests run in parallel, and a failure names the feature that broke.

use std::future::Future;
use std::panic::AssertUnwindSafe;

use super::tests::test_client;
use super::*;

/// Gives the test ids clear of every other test and run (pods, claims and
/// labels are named after them, in a namespace they all share) and a
/// sandbox manager of its own, which `get()` returns on the test's thread.
/// Returns the manager's client.
pub(super) async fn own_sandbox(pool: &PgPool) -> kube::Client {
    db::test_support::start_ids_clear_of_other_runs(pool)
        .await
        .expect("ids clear of other runs");
    let client = test_client().await;
    use_test_manager(client.clone());
    client
}

/// Runs `scenario` for at most `limit`, then deletes everything the test's
/// database made in the cluster, whether it passed, failed or ran out of
/// time, and only then reports how it went.
pub(super) async fn run_then_tear_down(
    pool: &PgPool,
    client: &kube::Client,
    limit: Duration,
    scenario: impl Future<Output = ()>,
) {
    // On the heap: a test thread's stack is small, and a scenario's future
    // holds every value it keeps across an await.
    let outcome = AssertUnwindSafe(Box::pin(tokio::time::timeout(limit, scenario)))
        .catch_unwind()
        .await;
    Box::pin(tear_down(pool, client)).await;
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(_)) => panic!("the scenario didn't finish within {limit:?}"),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// Deletes every conversation's pods and claims, and every volume's claim,
/// in the test's database. Best-effort: a failure here mustn't hide the
/// scenario's own.
async fn tear_down(pool: &PgPool, client: &kube::Client) {
    let Ok(instance) = db::smelt_instance(pool).await else {
        eprintln!("couldn't read the test database's instance; its cluster objects are left");
        return;
    };
    let conversations: Vec<i64> = sqlx::query_scalar("SELECT id FROM conversations")
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    let pods: Vec<(i64, i64)> = sqlx::query_as("SELECT conversation_id, id FROM sandbox_pods")
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    for conversation_id in conversations {
        let pod_ids: Vec<i64> = pods.iter().filter(|(c, _)| *c == conversation_id).map(|(_, p)| *p).collect();
        teardown_conversation_with(client, conversation_id, &pod_ids, &instance).await;
    }
    for volume in db::list_sandbox_volumes(pool).await.unwrap_or_default() {
        delete_volume_claim(client, volume.id, &instance).await.ok();
    }
}

/// Creates and sends a command in one step, waits for it to finish,
/// returns its `command_id`.
pub(super) async fn run_and_wait(
    pool: &PgPool,
    conversation_id: i64,
    terminal_id: i64,
    command_id: &str,
    command: &str,
) -> String {
    db::create_terminal_command(pool, conversation_id, terminal_id, command_id, command)
        .await
        .expect("create_terminal_command");
    send_command(pool, terminal_id, command_id, command)
        .await
        .expect("send_command");
    poll_until_finished(pool, command_id).await;
    command_id.to_string()
}

pub(super) async fn first_stdout_line(pool: &PgPool, command_id: &str) -> String {
    let lines = db::read_terminal_output(pool, command_id, &["stdout"], 0, 1)
        .await
        .expect("read_terminal_output");
    lines.first().map(|l| l.data.clone()).unwrap_or_default()
}

pub(super) async fn poll_until_finished(pool: &PgPool, command_id: &str) -> db::TerminalCommandStatus {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let status = db::terminal_command_status(pool, command_id)
            .await
            .expect("terminal_command_status")
            .expect("command should exist");
        if status.status != "running" {
            return status;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "command {command_id} did not finish in time"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Whether a `PodsChanged` arrives on `rx` within a few seconds,
/// skipping any older ones still queued.
pub(super) async fn received_pods_changed(
    rx: &mut tokio::sync::broadcast::Receiver<events::AppEvent>,
) -> bool {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(events::AppEvent::PodsChanged) => return true,
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

pub(super) async fn any_message_contains(pool: &PgPool, conversation_id: i64, needle: &str) -> bool {
    db::list_messages(pool, conversation_id)
        .await
        .expect("list_messages")
        .iter()
        .any(|m| {
            m.blocks().ok().is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|b| matches!(b, crate::anthropic::ContentBlock::Text { text } if text.contains(needle)))
            })
        })
}
