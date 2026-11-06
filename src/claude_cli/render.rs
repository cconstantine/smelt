//! What smelt sends a CLI run of its history (SME-430): the run holds the
//! model's context, so each call sends only what's new since the run's
//! last call (`tail_after`), and a new run gets smelt's history as text
//! (`rehydrate`), since the CLI takes no assistant messages on stdin.

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

/// A tool result as the model reads it in text: which call it answers,
/// and what came back.
pub fn tool_result_text(tool_use_id: &str, content: &str, is_error: bool) -> String {
    let failed = if is_error { ", which failed" } else { "" };
    format!("[The result of tool call {tool_use_id}{failed}:]\n{content}")
}

/// One message's blocks as text: thinking left out, tool calls and
/// results as marked lines.
pub fn blocks_text(blocks: &[ContentBlock]) -> String {
    let parts: Vec<String> = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } | ContentBlock::CompactionPlaceholder { text } => Some(text.clone()),
            ContentBlock::CompactionSummary { summary, .. } => Some(summary.clone()),
            ContentBlock::ToolUse { id, name, input } => Some(format!("[Tool call {id}: {name} {input}]")),
            ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                Some(tool_result_text(tool_use_id, content, is_error.unwrap_or(false)))
            }
            ContentBlock::Thinking { .. } => None,
        })
        .filter(|text| !text.trim().is_empty())
        .collect();
    parts.join("\n\n")
}

/// The messages as one text, each under its role.
fn transcript(messages: &[AnthropicMessage]) -> Vec<String> {
    messages.iter().map(|m| format!("### {}\n{}", m.role, blocks_text(&m.content))).collect()
}

/// What a new run's first message says before the history.
pub const REHYDRATED: &str = "smelt is starting a new Claude Code session for this conversation, so the conversation so far follows as a transcript. Its tool calls ran through smelt's tools, which are yours too. Carry on from where it ends.";

/// A new run's first message: the history before the trailing user
/// messages as a transcript (cut from the oldest end to fit half of
/// `window` tokens, at 4 characters a token, and saying so), then those
/// messages. With no history, only those messages.
pub fn rehydrate(messages: &[AnthropicMessage], window: u32) -> String {
    let (history, tail) = split_tail(messages);
    let new = tail.iter().map(|m| blocks_text(&m.content)).collect::<Vec<_>>().join("\n\n");
    if history.is_empty() {
        return new;
    }
    let budget = (window as usize / 2).saturating_mul(4).saturating_sub(new.len());
    let mut kept = transcript(history);
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
