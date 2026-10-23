//! `start_agent`: the checks and the transaction that make a child, then
//! its start, a spawned task that ends with the child `active` and woken,
//! or `failed` with the error as its report.

use super::*;

/// `start_agent`'s run: checks the input, records the child (`starting`)
/// with its brief as its first message, spawns its start, and returns the
/// child's id at once.
pub(crate) async fn start_agent(pool: &PgPool, caller: i64, input: &serde_json::Value) -> Result<String, String> {
    let brief = input
        .get("brief")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing or non-string field: brief")?;
    let chars = brief.chars().count();
    if brief.trim().is_empty() || chars > MAX_BRIEF_CHARS {
        return Err(format!("brief must be 1 to {MAX_BRIEF_CHARS} characters (it has {chars})"));
    }
    // The parent's model, resolved now: the child runs on it.
    let model = crate::providers::conversation_model(pool, caller)
        .await
        .map_err(|e| format!("couldn't read this conversation's model: {e}"))?
        .as_ref()
        .and_then(crate::providers::ConversationModel::choice)
        .map(|choice| (choice.provider_id, choice.model.clone()));
    let new = db::NewChild { caller, brief, persona_name: None, model };
    let child = match db::create_child(pool, &new).await.map_err(|e| format!("couldn't start the agent: {e}"))? {
        db::CreateChild::Created(child) => child,
        db::CreateChild::Refused(why) => return Err(why),
    };
    crate::events::publish_app(crate::events::AppEvent::ConversationsChanged);
    let start_pool = pool.clone();
    // In the tool call's span, so the start is its child.
    tokio::spawn(tracing::Instrument::in_current_span(async move {
        run_start(&start_pool, child, caller).await;
    }));
    Ok(format!(
        "Started {} (no persona). It works on its brief in its own conversation; its report \
         arrives here as a message when it calls finish, and list_agents shows its status.",
        label(child, None)
    ))
}

/// The start notice, the child's own: what its first turn drains.
fn start_notice() -> String {
    "Your start is done: work on the brief above. When your work is done, call finish with your \
     report. The user can also write to you here."
        .to_string()
}

/// The child's start: ends with it `active` and its first turn woken, or
/// `failed` (`fail_start`). A child stopped or deleted meanwhile is left
/// as it is. `StartExit` cleans up on every way out, a dropped task too.
async fn run_start(pool: &PgPool, child: i64, parent: i64) {
    let _exit = StartExit { pool: pool.clone(), child };
    if let Err(error) = prepare(parent).await {
        fail_start(pool, child, parent, &error).await;
        return;
    }
    #[cfg(test)]
    test_hooks::reach(test_hooks::BEFORE_START_DONE, parent).await;
    match db::complete_start(pool, child, parent, &start_notice()).await {
        Ok(true) => {
            crate::turn::notify(pool, child, vec![crate::turn::Notice::Wake]);
        }
        Ok(false) => tracing::info!(conversation_id = child, "the agent was stopped or deleted during its start"),
        Err(e) => fail_start(pool, child, parent, &format!("couldn't complete the start: {e}")).await,
    }
}

/// The start's steps before its done step. None yet without a repo; a
/// test can make it fail.
async fn prepare(parent: i64) -> Result<(), String> {
    #[cfg(test)]
    if let Some(error) = test_hooks::injected_start_failure(parent) {
        return Err(error);
    }
    let _ = parent;
    Ok(())
}

/// Ends a start that failed: the child `failed`, the error queued to its
/// parent as its report and saved in its own transcript, then the parent
/// woken. A child no longer starting (stopped or deleted meanwhile) gets
/// nothing.
pub(crate) async fn fail_start(pool: &PgPool, child: i64, parent: i64, error: &str) {
    let label = label(child, None);
    let text = format!("{label}'s start failed: {error}");
    match db::fail_start(pool, child, parent, &label, &text).await {
        Ok(true) => {
            crate::turn::notify(pool, parent, vec![crate::turn::Notice::Wake]);
            crate::events::publish_app(crate::events::AppEvent::ConversationsChanged);
        }
        Ok(false) => tracing::info!(conversation_id = child, "a failed start's agent was stopped or deleted meanwhile"),
        Err(e) => tracing::warn!(conversation_id = child, error = %e, start_error = %error, "couldn't record a failed start"),
    }
}

/// Held by a start from its first step: on every way out (done, failed,
/// the child stopped or deleted, the task dropped) it deletes any AGENTS.md
/// trust card in the child and removes the start's credentials, in a task
/// of its own, as `CloneGuard`'s cleanup runs.
struct StartExit {
    pool: PgPool,
    child: i64,
}

impl Drop for StartExit {
    fn drop(&mut self) {
        let pool = self.pool.clone();
        let child = self.child;
        tokio::spawn(async move {
            end_start(&pool, child).await;
        });
    }
}

/// What every way out of a start leaves done: no trust card left in the
/// child, and the start's credentials removed.
pub(crate) async fn end_start(pool: &PgPool, child: i64) {
    match db::delete_instruction_requests_of(pool, child).await {
        Ok(0) => {}
        Ok(_) => crate::git::publish_repos(pool, child).await,
        Err(e) => tracing::warn!(conversation_id = child, error = %e, "couldn't delete an agent's trust cards"),
    }
    drop_start_credentials(child).await;
    #[cfg(test)]
    test_hooks::start_ended(child);
}

/// Removes the SSH key from a child that mustn't keep it. Every child
/// keeps it until SME-242 says which mustn't (`agents.credentials`), as
/// every pod does today; the exits that call this are built here.
async fn drop_start_credentials(child: i64) {
    let _ = child;
}
