//! What an agent sends its parent (its report, `finish`, or a notice that
//! its turn ended without one), and what stops one: its parent's
//! `stop_agent`, or the user's Stop, which tells the parent.

use super::*;

/// `finish`'s run: the child's report queued to its parent, the child
/// `finished`, and the parent woken. The turn loop ends the child's turn
/// after a successful call (`turn::run_tools`).
pub(crate) async fn finish(pool: &PgPool, child: i64, input: &serde_json::Value) -> Result<String, String> {
    let report = input
        .get("report")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing or non-string field: report")?;
    let chars = report.chars().count();
    if report.trim().is_empty() || chars > MAX_REPORT_CHARS {
        return Err(format!("report must be 1 to {MAX_REPORT_CHARS} characters (it has {chars})"));
    }
    let outcome = db::finish_agent(pool, child, &label_of(pool, child).await, report)
        .await
        .map_err(|e| format!("couldn't send your report: {e}"))?;
    match outcome {
        db::Finish::Queued { parent } => {
            crate::turn::notify(pool, parent, vec![crate::turn::Notice::Wake]);
            Ok("Your report was sent to your parent. Your turn ends here; the user may still write to you.".to_string())
        }
        db::Finish::MessageWaiting => Err("a message arrived since your last step; read it, then finish".to_string()),
        db::Finish::NotActive(Some(state)) if state == db::AGENT_FINISHED => {
            Err("you've already finished: your report was sent".to_string())
        }
        db::Finish::NotActive(Some(state)) if state == db::AGENT_STOPPED => {
            Err("you were stopped by your parent, so there's no one to report to".to_string())
        }
        db::Finish::NotActive(state) => Err(format!(
            "you can't finish now (your state is {})",
            state.as_deref().unwrap_or("unknown")
        )),
    }
}

/// `stop_agent`'s run: one of the caller's children and everything under
/// it set `stopped`, then their turns stopped. Pods and terminals are left
/// alone, as the user's Stop leaves them.
pub(crate) async fn stop_agent(pool: &PgPool, caller: i64, input: &serde_json::Value) -> Result<String, String> {
    let id = input
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .ok_or("missing or non-integer field: id")?;
    let outcome = db::stop_subtree(pool, caller, id)
        .await
        .map_err(|e| format!("couldn't stop agent #{id}: {e}"))?;
    match outcome {
        db::StopSubtree::NotYourChild => Err(format!("#{id} isn't one of your agents (list_agents lists them)")),
        db::StopSubtree::AlreadyEnded(state) => Ok(format!("agent #{id} was already {state}")),
        db::StopSubtree::Stopped(subtree) => {
            for conversation_id in &subtree {
                crate::turn::stop_conversation(pool, *conversation_id).await;
            }
            // Their status changed in every tab's sidebar, a turn running
            // or not: a stopped turn's end says nothing for an idle one
            // (SME-246).
            crate::events::publish_app(crate::events::AppEvent::ConversationsChanged);
            Ok(match subtree.len() - 1 {
                0 => format!("Stopped agent #{id}."),
                1 => format!("Stopped agent #{id} and 1 agent under it."),
                under => format!("Stopped agent #{id} and {under} agents under it."),
            })
        }
    }
}

/// The user's Stop on `conversation_id`. Refused for a child still
/// starting, which its parent's `stop_agent` or a delete ends instead.
/// Stopping an `active` child that wasn't already paused tells its parent,
/// which would otherwise wait for a report that won't come.
pub(crate) async fn user_stop(pool: &PgPool, conversation_id: i64) -> Result<(), String> {
    let state = match db::agent_state(pool, conversation_id).await {
        Ok(state) => state,
        Err(e) => {
            // The stop still goes through; only the parent's notice is lost.
            tracing::warn!(conversation_id, error = %e, "couldn't read an agent's state at a Stop");
            None
        }
    };
    if state.as_deref() == Some(db::AGENT_STARTING) {
        return Err("This agent is still starting: stop it with stop_agent from its parent, or delete it.".to_string());
    }
    let was_paused = crate::turn::is_paused(conversation_id);
    crate::turn::stop_conversation(pool, conversation_id).await;
    if state.as_deref() == Some(db::AGENT_ACTIVE) && !was_paused {
        user_stopped(pool, conversation_id).await;
    }
    Ok(())
}

/// Tells an agent's parent the user stopped it, and wakes the parent.
async fn user_stopped(pool: &PgPool, child: i64) {
    let parent = match db::conversation_parent(pool, child).await {
        Ok(Some(parent)) => parent,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(conversation_id = child, error = %e, "couldn't find a stopped agent's parent");
            return;
        }
    };
    let label = label_of(pool, child).await;
    let body = format!(
        "the user stopped {label}; it carries on if the user writes to it, or call stop_agent to end it"
    );
    match db::queue_notice(pool, parent, child, &label, &body).await {
        Ok(()) => {
            crate::turn::notify(pool, parent, vec![crate::turn::Notice::Wake]);
        }
        Err(e) => tracing::warn!(conversation_id = child, error = %e, "couldn't tell a parent the user stopped its agent"),
    }
}

/// After a child's last turn ends (`failure`: why, when it failed): if it's
/// `active` with nothing to wait on and not paused by the user, its parent
/// is told its turn ended without a report, with the start of its last
/// reply, and woken; otherwise the parent would wait for a report that
/// won't come. A conversation that isn't an agent does nothing. Something
/// arriving just after the check wakes the child again, which is fine: the
/// notice says the turn ended, not the work.
pub(crate) async fn after_last_turn(pool: &PgPool, conversation_id: i64, failure: Option<String>) {
    let idle = db::idle_agent_without_report(pool, conversation_id).await;
    #[cfg(test)]
    let _checked = test_hooks::Checked(conversation_id);
    let parent = match idle {
        Ok(Some(idle)) if !crate::turn::is_paused(conversation_id) => idle,
        Ok(_) => return,
        Err(e) => {
            tracing::warn!(conversation_id, error = %e, "couldn't check whether an agent's turn ended without a report");
            return;
        }
    };
    let (parent, last) = parent;
    let label = label_of(pool, conversation_id).await;
    let failed = failure.map(|why| format!(" (it failed: {why})")).unwrap_or_default();
    let body = format!("{label}'s turn ended without a report{failed}; its last message: {last}");
    match db::queue_notice(pool, parent, conversation_id, &label, &body).await {
        Ok(()) => {
            crate::turn::notify(pool, parent, vec![crate::turn::Notice::Wake]);
        }
        Err(e) => tracing::warn!(conversation_id, error = %e, "couldn't tell a parent its agent's turn ended"),
    }
}
