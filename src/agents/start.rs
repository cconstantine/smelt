//! `start_agent`: the checks and the transaction that make a child, then
//! its start, a spawned task that ends with the child `active` and woken.

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
         arrives here as a message when it's done, and list_agents shows its status.",
        label(child, None)
    ))
}

/// The start notice, the child's own: what its first turn drains.
fn start_notice() -> String {
    "Your start is done: work on the brief above. When your work is done, call finish with your \
     report. The user can also write to you here."
        .to_string()
}

/// The child's start: ends with it `active` and its first turn woken. A
/// child stopped or deleted meanwhile is left as it is.
async fn run_start(pool: &PgPool, child: i64, parent: i64) {
    #[cfg(test)]
    test_hooks::reach(test_hooks::BEFORE_START_DONE, parent).await;
    match db::complete_start(pool, child, parent, &start_notice()).await {
        Ok(true) => {
            crate::turn::notify(pool, child, vec![crate::turn::Notice::Wake]);
        }
        Ok(false) => tracing::info!(conversation_id = child, "the agent was stopped or deleted during its start"),
        Err(e) => tracing::warn!(conversation_id = child, error = %e, "couldn't complete an agent's start"),
    }
    #[cfg(test)]
    test_hooks::start_ended(child);
}
