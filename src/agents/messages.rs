//! A parent and its child writing to each other (SME-238): `message_agent`,
//! and a child's `ask_parent`, which its parent's next message answers.

use super::*;

/// `message_agent`'s `to`: `"parent"`, or a child's id as a number or a
/// string of digits (`"12"`, `"#12"`).
fn recipient(input: &serde_json::Value) -> Result<db::Recipient, String> {
    const WHICH: &str = "to must be \"parent\" or the id of one of your agents (list_agents lists them)";
    match input.get("to") {
        Some(serde_json::Value::String(to)) if to.trim() == "parent" => Ok(db::Recipient::Parent),
        Some(serde_json::Value::String(to)) => {
            to.trim().trim_start_matches('#').parse::<i64>().map(db::Recipient::Child).map_err(|_| WHICH.to_string())
        }
        Some(serde_json::Value::Number(to)) => to.as_i64().map(db::Recipient::Child).ok_or_else(|| WHICH.to_string()),
        _ => Err(WHICH.to_string()),
    }
}

/// `message_agent`'s run: `text` queued to the caller's parent or one of
/// its own children, which is woken (a paused one only once the user
/// writes to it; a busy one takes it at its turn's next step).
pub(crate) async fn message_agent(pool: &PgPool, caller: i64, input: &serde_json::Value) -> Result<String, String> {
    let to = recipient(input)?;
    let text = input
        .get("text")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing or non-string field: text")?;
    let chars = text.chars().count();
    if text.trim().is_empty() || chars > MAX_MESSAGE_CHARS {
        return Err(format!("text must be 1 to {MAX_MESSAGE_CHARS} characters (it has {chars})"));
    }
    let label = match to {
        db::Recipient::Parent => label_of(pool, caller).await,
        db::Recipient::Child(_) => "your parent".to_string(),
    };
    let outcome = db::message_agent(pool, caller, to, &label, text)
        .await
        .map_err(|e| format!("couldn't send the message: {e}"))?;
    match outcome {
        db::MessageOutcome::Queued(recipient) => {
            // A finished recipient is `active` again; a paused one runs no
            // turn whose start would say so to other tabs (SME-246).
            crate::events::publish_app(crate::events::AppEvent::ConversationsChanged);
            crate::turn::notify(pool, recipient, vec![crate::turn::Notice::Wake]);
            Ok(match to {
                db::Recipient::Parent => "Sent to your parent.".to_string(),
                db::Recipient::Child(id) => format!("Sent to agent #{id}."),
            })
        }
        db::MessageOutcome::Answered(child) => {
            // No longer waiting for its parent, turn or not (SME-246).
            crate::events::publish_app(crate::events::AppEvent::ConversationsChanged);
            crate::turn::notify(pool, child, vec![crate::turn::Notice::Wake]);
            Ok(format!("Agent #{child} was waiting for your answer: it has it now."))
        }
        db::MessageOutcome::NoParent => Err("you have no parent: to is \"parent\" only for an agent".to_string()),
        db::MessageOutcome::NotYourChild(id) => Err(format!("#{id} isn't one of your agents (list_agents lists them)")),
        db::MessageOutcome::Deleted(id) => Err(format!("agent #{id} was deleted")),
        db::MessageOutcome::Ended(id, state) if state == db::AGENT_FAILED => Err(format!("agent #{id}'s start failed")),
        db::MessageOutcome::Ended(id, state) => Err(format!(
            "agent #{id} is {state}; it takes no messages unless the user writes to it"
        )),
    }
}

/// `ask_parent`'s call `tool_use_id` by `child`, from the turn
/// (`turn::run_tools`): the question recorded and queued to its parent,
/// which is woken. `Ok` ends the child's turn, as `ask_user` does; an
/// error is the call's result, and the turn goes on.
pub(crate) async fn ask_parent(pool: &PgPool, child: i64, tool_use_id: &str, input: &serde_json::Value) -> Result<(), String> {
    let question = input
        .get("question")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing or non-string field: question")?;
    let chars = question.chars().count();
    if question.trim().is_empty() || chars > MAX_MESSAGE_CHARS {
        return Err(format!("question must be 1 to {MAX_MESSAGE_CHARS} characters (it has {chars})"));
    }
    let label = label_of(pool, child).await;
    let outcome = db::ask_parent(pool, child, tool_use_id, question, &label)
        .await
        .map_err(|e| format!("couldn't ask your parent: {e}"))?;
    match outcome {
        db::AskParent::Asked { parent } => {
            crate::turn::notify(pool, parent, vec![crate::turn::Notice::Wake]);
            Ok(())
        }
        db::AskParent::NoParent => Err(format!("{ASK_PARENT} is only for an agent")),
        db::AskParent::NotActive(Some(state)) if state == db::AGENT_STOPPED => {
            Err("you were stopped by your parent, so there's no one to ask".to_string())
        }
        db::AskParent::NotActive(state) => Err(format!(
            "you can't ask your parent now (your state is {})",
            state.as_deref().unwrap_or("unknown")
        )),
        db::AskParent::ParentEnded(why) => Err(format!("your parent takes no questions: it's {why}")),
    }
}
