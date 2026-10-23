//! A parent and its child writing to each other (SME-238): `message_agent`.

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
            crate::turn::notify(pool, recipient, vec![crate::turn::Notice::Wake]);
            Ok(match to {
                db::Recipient::Parent => "Sent to your parent.".to_string(),
                db::Recipient::Child(id) => format!("Sent to agent #{id}."),
            })
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
