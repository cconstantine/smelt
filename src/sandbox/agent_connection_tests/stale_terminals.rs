//! Commands sent to terminals of ended pods (SME-425): a terminal whose
//! row is closed, or whose pod's row has ended, must be refused up front,
//! naming the conversation's live pod so the model is never sent to
//! `create_pod` (or on to `terminate_pod`) while one runs.
//!
//! Tests assert error *strings* only, so they compile against the unfixed
//! tree (the plan's red-first rule): the unfixed code answers the same
//! calls with `AgentUnreachable`'s create_pod advice, reached through the
//! `GoneDialer` below.

use super::*;

/// A dialer for a pod the cluster reports gone: every dial is refused,
/// and `death_reason` confirms it dead (with no Kubernetes reason). For an
/// ended pod this is what `ClusterDialer` would answer; the unfixed code
/// reaches it, the fixed code never dials.
struct GoneDialer;

impl AgentDialer for GoneDialer {
    fn dial(&self, _pod_id: i64) -> BoxFuture<'_, Result<Box<dyn AgentIo>, SandboxError>> {
        Box::pin(async {
            Err(SandboxError::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "the pod is gone",
            )))
        })
    }

    fn is_running(&self, _pod_id: i64) -> BoxFuture<'_, Result<bool, SandboxError>> {
        Box::pin(async { Ok(false) })
    }

    fn death_reason(&self, _pod_id: i64) -> BoxFuture<'_, Option<Option<String>>> {
        Box::pin(async { Some(None) })
    }

    fn image(&self, _pod_id: i64) -> BoxFuture<'_, Option<String>> {
        Box::pin(async { None })
    }
}

/// Puts a `GoneDialer` behind `dialer_for(pod_id)`, as `fake_agent` does
/// its fake agent.
fn gone_dialer(pod_id: i64) {
    test_dialers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(pod_id, Arc::new(GoneDialer));
}

/// A conversation with two pods: `pod_row`'s, ended, and a second one live
/// (the table keeps at most one live pod per conversation, so the first
/// ends before the second can exist) — the incident's shape: terminals of
/// a stopped pod touched while a live pod runs.
async fn two_pods(pool: &PgPool) -> (i64, i64, i64) {
    let (conversation_id, first_pod) = pod_row(pool).await;
    db::terminate_sandbox_pod(pool, first_pod).await.expect("end the first pod");
    let second_pod = db::create_sandbox_pod(pool, conversation_id).await.expect("second pod row").id;
    (conversation_id, first_pod, second_pod)
}

/// The refusal a stale terminal must give, both pods named, no pod advice
/// pointing at `create_pod`/`terminate_pod` while a live pod runs.
fn assert_names_both_pods(message: &str, terminal_id: i64, old_pod: i64, live_pod: i64) {
    for wanted in [
        &format!("terminal {terminal_id} belonged to pod {old_pod}, which has stopped"),
        &format!("pod {live_pod} is running"),
        "list_terminals",
        "create_terminal",
    ] {
        assert!(message.contains(wanted), "{wanted:?} missing from: {message}");
    }
    for forbidden in ["create_pod", "terminate_pod"] {
        assert!(!message.contains(forbidden), "{forbidden} must not be advised: {message}");
    }
}

/// The incident itself: a terminal of the conversation's old, stopped pod,
/// touched while a new pod runs. Refused naming both pods, and nothing
/// written — no `terminal_commands` row, so no panel flicker and no `lost`
/// row behind it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_run_command_on_a_stopped_pods_terminal_names_both_pods_and_never_says_create_pod(pool: PgPool) {
    let (conversation, old_pod, live_pod) = two_pods(&pool).await;
    gone_dialer(old_pod);
    let terminal = db::create_sandbox_terminal(&pool, old_pod).await.expect("terminal").id;
    db::terminate_sandbox_terminal(&pool, terminal).await.expect("close terminal");
    db::terminate_sandbox_pod(&pool, old_pod).await.expect("end the old pod");

    let message = match run_command(&pool, conversation, terminal, "cmd-stale-r1", "echo hi", 300).await {
        Ok(()) => panic!("a command on a stopped pod's terminal was accepted"),
        Err(RunCommandError::Busy(command)) => panic!("refused as busy, not as stale: {}", command.command_id),
        Err(RunCommandError::Failed(message)) => message,
    };
    assert_names_both_pods(&message, terminal, old_pod, live_pod);
    assert!(
        db::list_terminal_commands(&pool, terminal, 10).await.expect("list commands").is_empty(),
        "a refused run wrote a command row"
    );
}

/// `send_signal` routes by the terminal's own pod id too (the audit's
/// second broken surface): the same stale terminal gets the same refusal.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_signal_to_a_command_on_a_stale_terminal_is_refused_the_same_way(pool: PgPool) {
    let (_conversation, old_pod, live_pod) = two_pods(&pool).await;
    gone_dialer(old_pod);
    let terminal = db::create_sandbox_terminal(&pool, old_pod).await.expect("terminal").id;
    db::terminate_sandbox_terminal(&pool, terminal).await.expect("close terminal");
    db::terminate_sandbox_pod(&pool, old_pod).await.expect("end the old pod");

    let message = send_signal(&pool, terminal, "cmd-stale-r2", "INT")
        .await
        .expect_err("a signal to a stale terminal is refused")
        .to_string();
    assert_names_both_pods(&message, terminal, old_pod, live_pod);
}

/// A terminal closed while its pod lives: `terminate_terminal` on it is
/// idempotent `Ok(())` today (Q3, unchanged), but a command on it is
/// refused, pointing at `create_terminal`.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_terminal_closed_on_the_live_pod_says_create_terminal(pool: PgPool) {
    let (conversation, pod) = pod_row(&pool).await;
    gone_dialer(pod);
    let terminal = db::create_sandbox_terminal(&pool, pod).await.expect("terminal").id;
    db::terminate_sandbox_terminal(&pool, terminal).await.expect("close terminal");

    let message = match run_command(&pool, conversation, terminal, "cmd-stale-r3", "echo hi", 300).await {
        Ok(()) => panic!("a command on a closed terminal was accepted"),
        Err(RunCommandError::Busy(command)) => panic!("refused as busy, not as stale: {}", command.command_id),
        Err(RunCommandError::Failed(message)) => message,
    };
    for wanted in [
        &format!("terminal {terminal} has been terminated"),
        &format!("pod {pod} is running"),
        "create_terminal",
    ] {
        assert!(message.contains(wanted), "{wanted:?} missing from: {message}");
    }
    for forbidden in ["create_pod", "terminate_pod"] {
        assert!(!message.contains(forbidden), "{forbidden} must not be advised: {message}");
    }
}

/// With no live pod there is no wrong advice to avoid: naming `create_pod`
/// is right and actionable (Q4, Decided).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stale_terminal_with_no_live_pod_points_at_create_pod(pool: PgPool) {
    let (conversation, pod) = pod_row(&pool).await;
    gone_dialer(pod);
    let terminal = db::create_sandbox_terminal(&pool, pod).await.expect("terminal").id;
    db::terminate_sandbox_pod(&pool, pod).await.expect("end the pod");

    let message = match run_command(&pool, conversation, terminal, "cmd-stale-r4", "echo hi", 300).await {
        Ok(()) => panic!("a command on an ended pod's terminal was accepted"),
        Err(RunCommandError::Busy(command)) => panic!("refused as busy, not as stale: {}", command.command_id),
        Err(RunCommandError::Failed(message)) => message,
    };
    for wanted in [
        &format!("terminal {terminal} belonged to pod {pod}, which has stopped"),
        "no running pod",
        "create_pod",
        "create_terminal",
    ] {
        assert!(message.contains(wanted), "{wanted:?} missing from: {message}");
    }
    assert!(
        db::list_terminal_commands(&pool, terminal, 10).await.expect("list commands").is_empty(),
        "a refused run wrote a command row"
    );
}
