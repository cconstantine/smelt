//! The sandbox's lifecycle against a real cluster, one feature per test
//! (SME-94): pods, terminals, commands, file tools, repos and volumes,
//! each run by the real agent in a real pod.
//!
//! Every test starts with `own_sandbox`, which gives it ids no other test
//! or run shares and a sandbox manager of its own behind `get()`, and runs
//! its scenario through `run_then_tear_down`, which bounds it in time and
//! deletes what its database made in the cluster, pass or fail. So the
//! tests run in parallel, and a failure names the feature that broke.
//!
//! The first `own_sandbox` in a process also sweeps the test namespace of
//! every pod and claim more than an hour old (`sweep`, SME-134): what a
//! killed run or a failed teardown left, which nothing else deletes.

use std::future::Future;
use std::panic::AssertUnwindSafe;

use super::tests::test_client;
use super::*;

mod sweep;

/// Gives the test ids clear of every other test and run (pods, claims and
/// labels are named after them, in a namespace they all share) and a
/// sandbox manager of its own, which `get()` returns on the test's thread.
/// Returns the manager's client. The first call in a process sweeps the
/// test namespace first (`sweep::sweep_once`); later ones wait for it.
async fn own_sandbox(pool: &PgPool) -> kube::Client {
    sweep::sweep_once().await;
    db::test_support::start_ids_clear_of_other_runs(pool)
        .await
        .expect("ids clear of other runs");
    let client = test_client().await;
    use_test_manager(client.clone());
    client
}

/// SME-134: the harness sweeps the namespace of old leftovers once per
/// process, however many tests start.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_own_sandbox_sweeps_once_per_process(pool: PgPool) {
    own_sandbox(&pool).await;
    own_sandbox(&pool).await;

    assert_eq!(sweep::sweeps_started(), 1);
}

/// How many of these tests run their scenarios at once. Each starts a pod
/// or more, with a Docker sidecar each; a dozen starting together on one
/// node took longer than a pod start may (90 s), failing these and the
/// other real-cluster tests alike. CI runs four tests at a time anyway.
static SCENARIOS_AT_ONCE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

/// Runs `scenario` for at most `limit`, then deletes everything the test's
/// database made in the cluster, whether it passed, failed or ran out of
/// time, and only then reports how it went. Waits its turn first (see
/// `SCENARIOS_AT_ONCE`), which `limit` doesn't count.
async fn run_then_tear_down(
    pool: &PgPool,
    client: &kube::Client,
    limit: Duration,
    scenario: impl Future<Output = ()>,
) {
    let _turn = SCENARIOS_AT_ONCE.acquire().await.expect("the semaphore is never closed");
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

/// How long teardown waits for a pod start to finish: a start can wait
/// out a stopping pod and then its own pod each for the running wait, then
/// do its git setup.
fn pod_start_wait() -> Duration {
    2 * running_wait_timeout() + Duration::from_secs(60)
}

/// SME-94 review 2: teardown's wait outlasts a pod start, which can wait
/// out a stopping pod and then its own pod each for the running wait (120
/// s in CI), then do its git setup.
#[test]
fn test_teardown_waits_longer_than_a_pod_start_can_take() {
    assert!(
        pod_start_wait() > 2 * running_wait_timeout(),
        "{:?} is shorter than two running waits of {:?}",
        pod_start_wait(),
        running_wait_timeout()
    );
}

/// Deletes every conversation's pods and claims, every volume's claim, and
/// anything else labelled with the test database's instance. Best-effort:
/// a failure here mustn't hide the scenario's own.
async fn tear_down(pool: &PgPool, client: &kube::Client) {
    let Ok(instance) = db::smelt_instance(pool).await else {
        eprintln!("couldn't read the test database's instance; its cluster objects are left");
        return;
    };
    let conversations: Vec<i64> = sqlx::query_scalar("SELECT id FROM conversations")
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    // `create_pod` starts a pod in a task of its own, holding the
    // conversation's start lock, which a cut-off scenario leaves running:
    // it would make its claims and pod after the deletes below (SME-94
    // review 1). Wait out each start running or queued, and keep the lock
    // for the rest of the test, so no start asked for later runs. Past the
    // wait, give up and say so. No other test shares these conversation
    // ids.
    for &conversation_id in &conversations {
        match tokio::time::timeout(pod_start_wait(), pod_start_lock(conversation_id).lock_owned()).await {
            Ok(held) => std::mem::forget(held),
            Err(_) => eprintln!("conversation {conversation_id}'s pod start outlasted {:?}", pod_start_wait()),
        }
    }
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
    // Anything else of this database's: a pod made outside a conversation
    // (`create_with_docker` with the database's instance), say.
    let ours = ListParams::default().labels(&format!("{INSTANCE_LABEL}={}", instance.id));
    let pods = pods_api(client);
    for name in pods.list(&ours).await.map(|l| l.items).unwrap_or_default().into_iter().filter_map(|p| p.metadata.name) {
        pods.delete(&name, &pod_delete_params()).await.ok();
    }
    let claims = pvc_api(client);
    for name in claims.list(&ours).await.map(|l| l.items).unwrap_or_default().into_iter().filter_map(|c| c.metadata.name) {
        claims.delete(&name, &DeleteParams::default()).await.ok();
    }
}

/// This database's pods and claims still in the cluster and not being
/// deleted, by name.
async fn kept_objects(pool: &PgPool, client: &kube::Client) -> Vec<String> {
    let instance = db::smelt_instance(pool).await.expect("instance");
    let ours = ListParams::default().labels(&format!("{INSTANCE_LABEL}={}", instance.id));
    let pods = pods_api(client).list(&ours).await.expect("list pods").items.into_iter().map(|p| p.metadata);
    let claims = pvc_api(client).list(&ours).await.expect("list claims").items.into_iter().map(|c| c.metadata);
    pods.chain(claims)
        .filter(|meta| meta.deletion_timestamp.is_none())
        .filter_map(|meta| meta.name)
        .collect()
}

/// SME-94 review 1: a scenario cut off while `create_pod`'s start runs in
/// its own task (here by the time limit) leaves nothing behind. Teardown
/// waits the start out and keeps its lock, so nothing the start makes
/// outlives the deletes.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_teardown_waits_out_a_pod_start_the_scenario_left_running(pool: PgPool) {
    let client = own_sandbox(&pool).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    // Cut off as soon as the start is spawned.
    let cut_off = AssertUnwindSafe(run_then_tear_down(&pool, &client, Duration::ZERO, async {
        let _ = create_pod(&pool, conversation.id, PodLimitOverrides::default()).await;
    }))
    .catch_unwind()
    .await;
    // A start still running would make its pod within seconds.
    tokio::time::sleep(Duration::from_secs(10)).await;
    let start_held = pod_start_lock(conversation.id).try_lock().is_err();
    let kept = kept_objects(&pool, &client).await;

    // This test's own cleanup, whatever happened above.
    for name in &kept {
        pods_api(&client).delete(name, &immediate_delete_params()).await.ok();
        pvc_api(&client).delete(name, &DeleteParams::default()).await.ok();
    }
    assert!(cut_off.is_err(), "the scenario should have run out of time");
    assert!(start_held, "teardown should keep the conversation's pod-start lock, so no start runs after it");
    assert!(kept.is_empty(), "teardown left a pod start's {kept:?}");
}

/// Creates and sends a command in one step, waits for it to finish,
/// returns its `command_id`.
async fn run_and_wait(
    pool: &PgPool,
    conversation_id: i64,
    terminal_id: i64,
    command_id: &str,
    command: &str,
) -> String {
    db::create_terminal_command(pool, conversation_id, terminal_id, command_id, command, 300)
        .await
        .expect("create_terminal_command");
    send_command(pool, terminal_id, command_id, command)
        .await
        .expect("send_command");
    poll_until_finished(pool, command_id).await;
    command_id.to_string()
}

async fn first_stdout_line(pool: &PgPool, command_id: &str) -> String {
    let lines = db::read_terminal_output(pool, command_id, &["stdout"], 0, 1)
        .await
        .expect("read_terminal_output");
    lines.first().map(|l| l.data.clone()).unwrap_or_default()
}

async fn poll_until_finished(pool: &PgPool, command_id: &str) -> db::TerminalCommandStatus {
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
async fn received_pods_changed(
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

async fn any_message_contains(pool: &PgPool, conversation_id: i64, needle: &str) -> bool {
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

/// How long a scenario may take unless it waits on purpose: a pod start or
/// two, under the load of the other tests' pods starting alongside.
const SCENARIO_LIMIT: Duration = Duration::from_secs(180);

/// Whether a message containing `needle` reaches `conversation_id` within
/// `within`.
async fn message_arrives(pool: &PgPool, conversation_id: i64, needle: &str, within: Duration) -> bool {
    tokio::time::timeout(within, async {
        while !any_message_contains(pool, conversation_id, needle).await {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .is_ok()
}

mod crashes;
mod create;
mod repos;
mod personas;
mod commands_and_files;
mod pods_and_terminals;
mod starting;
mod volumes;
