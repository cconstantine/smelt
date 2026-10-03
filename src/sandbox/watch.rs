//! The pod watcher: noticing pods that finish, crash or vanish, and
//! cleaning up after them.

use super::*;

/// How old a live pod row must be before the watch's listing may close it
/// for having no pod in Kubernetes: a row is written just before its pod
/// is created, and creating one can take a while. Also keeps a smelt
/// instance from closing another instance's brand-new rows (the browser
/// test harness shares the dev database but not its namespace).
pub(super) const RECONCILE_MIN_AGE_SECS: i64 = 300;

/// How long after a pod is deleted (or finishes) the watch waits before
/// closing its record, so crash detection gets there first for a pod smelt
/// was connected to: it closes the same records, but also tells the model.
#[cfg(not(test))]
pub(super) const CLOSE_GRACE: Duration = Duration::from_secs(30);
#[cfg(test)]
pub(super) const CLOSE_GRACE: Duration = Duration::from_secs(1);

/// Keeps pod records in step with Kubernetes for as long as smelt runs:
/// subscribes to pod changes in smelt's namespace, reconciles once the
/// first full listing arrives (and again after any reconnect, which
/// re-lists), then closes a pod's record when the pod is deleted or reaches
/// a phase it can't recover from. Covers pods lost while smelt wasn't
/// connected to them: deleted outside smelt, lost in a cluster rebuild,
/// evicted, or dead while smelt was down. See `close_if_gone`.
///
/// Only a real server runs this (from `main`), never the browser test
/// harness: that shares the dev database but works in the test namespace,
/// where the dev instance's pods don't exist.
pub async fn watch_pods(pool: PgPool) {
    use futures_util::StreamExt;
    use kube::runtime::{WatchStreamExt, watcher};

    let client = match get() {
        Ok(manager) => manager.client.clone(),
        Err(e) => {
            tracing::error!(error = %e, "can't watch pods");
            return;
        }
    };
    let events = watcher(pods_api(&client), watcher::Config::default()).default_backoff();
    futures_util::pin_mut!(events);
    // Pods listed since the last `Init`, until `InitDone` completes the set.
    let mut listed = std::collections::HashSet::new();
    // Each pod's Docker sidecar, see `note_docker_restarts`.
    let mut docker_restarts = HashMap::new();
    // Server pods whose stop was seen, see `lsp::pods::note_server_stop`.
    let mut stopped_servers = std::collections::HashSet::new();
    while let Some(event) = events.next().await {
        match event {
            Ok(watcher::Event::Init) => listed.clear(),
            Ok(watcher::Event::InitApply(pod)) => {
                if let Some(restart) = note_docker_restarts(&mut docker_restarts, &pod, true) {
                    handle_docker_restart(&pool, restart).await;
                }
                crate::lsp::pods::note_server_stop(&mut stopped_servers, &pod, true);
                if let Some(pod_id) = watched_pod_id(&pod) {
                    if !pod_has_finished(&pod) {
                        listed.insert(pod_id);
                    }
                }
            }
            Ok(watcher::Event::InitDone) => {
                forget_unlisted_docker(&mut docker_restarts, &listed);
                match db::live_pods_older_than(&pool, RECONCILE_MIN_AGE_SECS).await {
                    Ok(rows) => {
                        for row in rows.into_iter().filter(|row| !listed.contains(&row.id)) {
                            close_if_gone(&pool, row.id).await;
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "couldn't list pod records to reconcile"),
                }
            }
            Ok(watcher::Event::Apply(pod)) if pod_has_finished(&pod) => {
                if let Some(stopped) = crate::lsp::pods::note_server_stop(&mut stopped_servers, &pod, false) {
                    let pool = pool.clone();
                    tokio::spawn(async move {
                        crate::turn::deliver_notice(&pool, stopped.conversation_id, stopped.notice).await;
                    });
                }
                if let Some(pod_id) = watched_pod_id(&pod) {
                    close_after_grace(pool.clone(), pod_id);
                }
            }
            Ok(watcher::Event::Apply(pod)) => {
                if let Some(restart) = note_docker_restarts(&mut docker_restarts, &pod, false) {
                    handle_docker_restart(&pool, restart).await;
                }
            }
            Ok(watcher::Event::Delete(pod)) => {
                if let Some(pod_id) = watched_pod_id(&pod) {
                    docker_restarts.remove(&pod_id);
                    close_after_grace(pool.clone(), pod_id);
                }
            }
            // The watcher backs off and retries on its own, re-listing
            // (a fresh `Init`..`InitDone`) once it's back.
            Err(e) => tracing::warn!(error = %e, "pod watch error"),
        }
    }
}

/// The smelt pod id behind a watched pod (`sandbox-{id}`), or `None` for
/// any other pod in the namespace.
pub(super) fn watched_pod_id(pod: &Pod) -> Option<i64> {
    pod.metadata.name.as_deref()?.strip_prefix("sandbox-")?.parse().ok()
}

/// Whether a pod has stopped for good: `Succeeded` or `Failed` (sandbox
/// pods never restart).
pub(super) fn pod_has_finished(pod: &Pod) -> bool {
    matches!(
        pod.status.as_ref().and_then(|s| s.phase.as_deref()),
        Some("Succeeded" | "Failed")
    )
}

/// `close_if_gone` for `pod_id`, after `CLOSE_GRACE`.
pub(super) fn close_after_grace(pool: PgPool, pod_id: i64) {
    tokio::spawn(async move {
        tokio::time::sleep(CLOSE_GRACE).await;
        close_if_gone(&pool, pod_id).await;
    });
}

/// Closes `pod_id`'s record if it's still live, smelt has no connection to
/// it (a connected pod's end is crash detection's to report, notice
/// included), and Kubernetes really has no running pod for it. Running
/// commands are marked lost and terminals closed, like a crash, but
/// quietly: no notice to the model (saving one would move the conversation
/// to the top of the sidebar, and a cluster rebuild can leave dozens); it
/// finds out if it tries the pod again.
pub(super) async fn close_if_gone(pool: &PgPool, pod_id: i64) {
    match db::sandbox_pod_is_live(pool, pod_id).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            tracing::warn!(pod_id, error = %e, "couldn't check a pod record");
            return;
        }
    }
    if registry_get(pod_id).is_some() {
        return;
    }
    let Ok(manager) = get() else {
        tracing::warn!(pod_id, "couldn't check a pod in Kubernetes: the sandbox isn't set up");
        return;
    };
    match pods_api(&manager.client).get_opt(&pod_name(pod_id)).await {
        Ok(Some(pod)) if !pod_has_finished(&pod) => return,
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(pod_id, error = %e, "couldn't check a pod in Kubernetes");
            return;
        }
    }
    close_pod_terminals(pool, pod_id).await;
    // Deletes a finished pod's object if it's still there, marks the row
    // terminated, and tells the UI (the sandbox panel, the sidebar, /pods).
    match force_terminate_pod(pool, pod_id).await {
        Ok(_) => tracing::info!(pod_id, "closed the record of a pod that's gone from the cluster"),
        Err(e) => tracing::warn!(pod_id, error = %e, "couldn't close the record of a gone pod"),
    }
}

/// Marks every command still `running` under any of this pod's terminals
/// `'lost'` (no real exit code to report — see
/// `db::mark_terminal_command_lost`), and every one of the pod's live
/// terminals terminated — a dead agent was hosting all of them, not just
/// one. A safe no-op if the pod had no terminals.
pub(super) async fn handle_crash_cleanup(pool: &PgPool, pod_id: i64, reason: Option<String>) {
    let (conversation_id, found_live_terminal) = close_pod_terminals(pool, pod_id).await;
    if let Some(conversation_id) = conversation_id {
        // One pod-level notification, gated on the same "found at least one
        // live terminal" condition that already makes a redundant second call
        // (e.g. the pre-existing reactive path still firing after this one
        // already ran) a harmless no-op — no separate dedup state needed. See
        // SME-12's "Detection design": the reason string, when Kubernetes
        // gave one, is passed straight through rather than guessed at.
        let notice = found_live_terminal.then(|| match reason {
            Some(reason) => format!(
                "Sandbox pod {pod_id} stopped unexpectedly ({reason}); every terminal running in it is no longer available."
            ),
            None => format!(
                "Sandbox pod {pod_id} stopped unexpectedly; every terminal running in it is no longer available."
            ),
        });
        // Then the same active wake as a normal command exit (see
        // `handle_agent_message`'s "exit" branch) — a crash can leave a
        // command marked 'lost' with nobody proactively telling the model,
        // the identical gap. One wake covers whatever this pass just
        // marked lost; `wake_conversation`'s own no-op-when-nothing-
        // pending behavior makes this cheap even when nothing actually
        // changed. Detached, notice included: this can run synchronously
        // from *inside* an already-in-progress `run_turn`/`execute()` call
        // that's already holding `conversation_id`'s lock (e.g.
        // `run_terminal_command_tool` → `sandbox::send_command` →
        // `reconnect_if_needed` → here), and both the notice (saved only
        // between turns, see `save_notice_between_turns`) and the wake
        // take that same non-reentrant lock. See
        // SME-13.
        let pool = pool.clone();
        tokio::spawn(async move {
            if let Some(notice) = notice {
                if let Err(e) = crate::turn::save_notice_between_turns(&pool, conversation_id, notice).await {
                    tracing::warn!(conversation_id, pod_id, error = %e, "couldn't save the pod crash notice");
                }
            }
            let _ = crate::turn::wake_conversation(&pool, conversation_id).await;
        });
    }
    deregister(pod_id);
}

/// Marks every command still running in `pod_id`'s terminals lost and
/// closes the terminals, publishing each closure for the UI. Returns the
/// pod's conversation (if it could be found) and whether it had any live
/// terminal. Shared by a crash and by the user stopping the pod.
pub(super) async fn close_pod_terminals(pool: &PgPool, pod_id: i64) -> (Option<i64>, bool) {
    let conversation_id = db::sandbox_pod_conversation_id(pool, pod_id)
        .await
        .ok()
        .flatten();
    let mut found_live_terminal = false;
    if let Ok(terminals) = db::list_sandbox_terminals_for_pod(pool, pod_id).await {
        for terminal in terminals {
            found_live_terminal = true;
            if let Ok(Some(running)) = db::terminal_command_is_running(pool, terminal.id).await {
                let _ = db::mark_terminal_command_lost(pool, &running.command_id).await;
            }
            let _ = db::terminate_sandbox_terminal(pool, terminal.id).await;
            if let Some(conversation_id) = conversation_id {
                events::publish(
                    conversation_id,
                    events::ConversationEvent::SandboxTerminalUpdate {
                        pod_id,
                        terminal_id: terminal.id,
                        status: "disconnected".to_string(),
                        terminated: true,
                    },
                );
            }
        }
    }
    (conversation_id, found_live_terminal)
}
