//! What smelt sends a CLI run of its history (SME-430): the run holds the
//! model's context, so each call sends only what's new since the run's
//! last call (`tail_after`), and a new run gets smelt's history as text
//! (`rehydrate`), since the CLI takes no assistant messages on stdin.

use std::collections::HashMap;

use sha2::{Digest, Sha256};

use crate::anthropic::{AnthropicMessage, ContentBlock};

/// What a run has been sent: the request's first `len` messages, by hash.
/// The model's reply to them is the message after.
#[derive(Clone, Debug, PartialEq)]
pub struct Synced {
    pub len: usize,
    pub hash: String,
}

fn hash(messages: &[AnthropicMessage]) -> String {
    let mut hasher = Sha256::new();
    for message in messages {
        // A message always serializes; one that somehow didn't would hash
        // as empty, and only make a later check miss.
        hasher.update(serde_json::to_vec(message).unwrap_or_default());
        hasher.update([0]);
    }
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// What a run has after a call with `messages`.
pub fn synced(messages: &[AnthropicMessage]) -> Synced {
    Synced { len: messages.len(), hash: hash(messages) }
}

/// The messages a run synced to `synced` hasn't had: everything after its
/// last call's messages and the reply to them, when the request still
/// starts with exactly those, the reply is next, and all that follows is
/// the user's side (tool results and notices). `None` when the history
/// went another way, and the run can't carry on from it.
pub fn tail_after<'a>(synced: &Synced, messages: &'a [AnthropicMessage]) -> Option<&'a [AnthropicMessage]> {
    let prefix = messages.get(..synced.len)?;
    if hash(prefix) != synced.hash {
        return None;
    }
    let (reply, tail) = messages.get(synced.len..)?.split_first()?;
    (reply.role == "assistant" && !tail.is_empty() && tail.iter().all(|m| m.role == "user")).then_some(tail)
}

/// The trailing run of user-role messages: what the model hasn't seen
/// yet, after everything before it.
pub fn split_tail(messages: &[AnthropicMessage]) -> (&[AnthropicMessage], &[AnthropicMessage]) {
    let start = messages.iter().rposition(|m| m.role != "user").map_or(0, |i| i + 1);
    messages.split_at(start)
}

/// Which tool each call in `messages` ran, by call id: a result names it.
pub fn call_names(messages: &[AnthropicMessage]) -> HashMap<String, String> {
    messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, .. } => Some((id.clone(), name.clone())),
            _ => None,
        })
        .collect()
}

/// A tool result as the model reads it in text, past tense and naming
/// its tool when known, so it can't read as a call to make (review 1 L9).
pub fn tool_result_text(tool_use_id: &str, name: Option<&str>, content: &str, is_error: bool) -> String {
    let what = if is_error { "it failed" } else { "it returned" };
    let ran = name.map_or_else(|| format!("smelt ran call {tool_use_id}"), |name| format!("smelt ran `{name}` (call {tool_use_id})"));
    format!("({ran}; {what}:)\n{}", neutralised(content))
}

/// A tool call as the model reads it in text: past tense, its input as
/// `key = value` pairs rather than a call's JSON (review 1 L9).
fn tool_call_text(id: &str, name: &str, input: &serde_json::Value) -> String {
    let given: Vec<String> = match input {
        serde_json::Value::Object(map) => map.iter().map(|(key, value)| format!("{key} = {value}")).collect(),
        serde_json::Value::Null => Vec::new(),
        other => vec![other.to_string()],
    };
    let with = if given.is_empty() { String::new() } else { format!(", with {}", given.join(", ")) };
    format!("(Earlier, you had smelt run `{name}` (call {id}){with}.)")
}

/// `text` with every line that starts like a transcript heading escaped,
/// so content can't fake a turn of the transcript (review 1 L9).
fn neutralised(text: &str) -> String {
    text.lines()
        .map(|line| if line.starts_with("###") { format!("\\{line}") } else { line.to_string() })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One message's blocks as text: thinking left out, tool calls and
/// results in the past tense, named through `names` (`call_names`).
pub fn blocks_text(blocks: &[ContentBlock], names: &HashMap<String, String>) -> String {
    let parts: Vec<String> = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } | ContentBlock::CompactionPlaceholder { text } => Some(neutralised(text)),
            ContentBlock::CompactionSummary { summary, .. } => Some(neutralised(summary)),
            ContentBlock::ToolUse { id, name, input } => Some(tool_call_text(id, name, input)),
            ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                Some(tool_result_text(tool_use_id, names.get(tool_use_id).map(String::as_str), content, is_error.unwrap_or(false)))
            }
            ContentBlock::Thinking { .. } => None,
        })
        .filter(|text| !text.trim().is_empty())
        .collect();
    parts.join("\n\n")
}

/// The messages as one text, each under its role.
fn transcript(messages: &[AnthropicMessage], names: &HashMap<String, String>) -> Vec<String> {
    messages.iter().map(|m| format!("### {}\n{}", m.role, blocks_text(&m.content, names))).collect()
}

/// What a new run's first message says before the history.
pub const REHYDRATED: &str = "smelt is starting a new Claude Code session for this conversation, so the conversation so far follows as a transcript. Its tool calls ran through smelt's tools, which are yours too: use them to act, and never write a tool call as text. Carry on from where it ends.";

/// A new run's first message: the history before the trailing user
/// messages as a transcript (cut from the oldest end to fit half of
/// `window` tokens, at 4 characters a token, and saying so), then those
/// messages. With no history, only those messages.
pub fn rehydrate(messages: &[AnthropicMessage], window: u32) -> String {
    let (history, tail) = split_tail(messages);
    let names = call_names(messages);
    let new = tail.iter().map(|m| blocks_text(&m.content, &names)).collect::<Vec<_>>().join("\n\n");
    if history.is_empty() {
        return new;
    }
    let budget = (window as usize / 2).saturating_mul(4).saturating_sub(new.len());
    let mut kept = transcript(history, &names);
    let mut size: usize = kept.iter().map(|part| part.len() + 2).sum();
    let mut dropped = 0;
    while size > budget && !kept.is_empty() {
        size -= kept.remove(0).len() + 2;
        dropped += 1;
    }
    let mut text = REHYDRATED.to_string();
    if dropped > 0 {
        text.push_str(&format!(" ({dropped} of its oldest messages are left out to fit.)"));
    }
    text.push_str("\n\n");
    text.push_str(&kept.join("\n\n"));
    text.push_str("\n\n### The new message\n");
    text.push_str(&new);
    text
}
