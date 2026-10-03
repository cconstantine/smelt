//! Compacting a conversation that nears its model's context window.

use super::*;

/// Extra headroom reserved on top of `MAX_TOKENS`, in case a smaller
/// `max_tokens` is ever configured per-request in the future — mirrors
/// opencode's own `max(output reserve, buffer)` term. Not currently
/// reachable as its own binding factor since `MAX_TOKENS` already exceeds
/// it, but kept as a named floor rather than assuming `MAX_TOKENS` always
/// will. See SME-18.
#[cfg(feature = "server")]
pub(super) const COMPACTION_SAFETY_BUFFER: u32 = 4096;

/// A cheap, approximate token-count estimate for content not yet sent —
/// chars/4, Anthropic's own rough published guidance, not a real
/// tokenizer. Used only to decide whether compaction should run before a
/// request goes out; never persisted or shown as if it were exact usage.
///
/// Spiked once against the real (non-mock) gateway configured in dev,
/// rather than left as a pure assumption: two consecutive real turns'
/// `usage.input_tokens` differ by (previous turn's real `output_tokens` +
/// whatever new message content was added) — isolating that second term
/// for an 800-character message gave ~211 real tokens against this
/// function's 200-token estimate, ~3.8 real chars/token vs. the 4.0
/// assumed here. Close enough not to revisit without a reason to — and
/// that one data point (repeated single characters) is a harder case for
/// a tokenizer than real prose, which should compress to *more*
/// chars/token, not fewer, so if anything this errs slightly generous
/// already. Same method (diff two real `get_context_usage` reads, subtract
/// the earlier one's `output_tokens`) works to re-check this later against
/// a real, varied conversation if the constant is ever in question.
#[cfg(feature = "server")]
pub(super) fn estimate_tokens(blocks: &[anthropic::ContentBlock]) -> u64 {
    let chars: usize = blocks
        .iter()
        .map(|block| match block {
            anthropic::ContentBlock::Text { text } => text.len(),
            anthropic::ContentBlock::ToolResult { content, .. } => content.len(),
            anthropic::ContentBlock::ToolUse { input, .. } => input.to_string().len(),
            anthropic::ContentBlock::Thinking {
                thinking,
                signature,
            } => thinking.len() + signature.len(),
            anthropic::ContentBlock::CompactionSummary { summary, .. } => summary.len(),
            anthropic::ContentBlock::CompactionPlaceholder { text } => text.len(),
        })
        .sum();
    (chars / 4) as u64
}

/// Whether the upcoming request's real size looks likely to eat into the
/// reserved ceiling (model output allowance + a safety buffer, subtracted
/// from the context window) — opencode's own trigger mechanism, checked
/// *before* a request is sent rather than reacting to a real rejection.
/// `last_usage` is the last real response's usage (the ground truth for
/// everything already in the conversation — see `stream::StreamedTurn`'s
/// own doc comment); `new_content_estimate` is `estimate_tokens` applied
/// to whatever's freshly being added this turn. `None` `last_usage`
/// (nothing sent yet) never triggers — there's nothing to compact. See
/// SME-18.
#[cfg(feature = "server")]
pub(super) fn should_compact(
    last_usage: Option<&anthropic::TokenUsage>,
    new_content_estimate: u64,
    context_window: u32,
) -> bool {
    let Some(last_usage) = last_usage else {
        return false;
    };
    let already_used = (last_usage.input_tokens
        + last_usage.output_tokens
        + last_usage.cache_creation_input_tokens
        + last_usage.cache_read_input_tokens)
        .max(0) as u64;
    let projected = already_used + new_content_estimate;
    let reserved = MAX_TOKENS.max(COMPACTION_SAFETY_BUFFER) as u64;
    let ceiling = (context_window as u64).saturating_sub(reserved);
    projected >= ceiling
}

/// Whether it's safe to end a compaction's covered range right after a
/// message with this content — i.e. it contains no `tool_use` block whose
/// paired `tool_result` (always the very next persisted message, in this
/// codebase's per-turn persistence model) hasn't been included too.
/// Anthropic rejects a request with an orphaned half outright, so this is
/// a hard constraint on where `covers_through_message_id` may point, never
/// best-effort. See SME-18.
#[cfg(feature = "server")]
pub(super) fn is_safe_compaction_boundary(blocks: &[anthropic::ContentBlock]) -> bool {
    !blocks
        .iter()
        .any(|block| matches!(block, anthropic::ContentBlock::ToolUse { .. }))
}

/// A fixed, structural placeholder — never shown as if the user actually
/// typed it, just what makes the synthetic post-compaction exchange below
/// start validly with `user` (Anthropic's own requirement, not negotiable).
#[cfg(feature = "server")]
pub(super) const COMPACTION_PLACEHOLDER_PROMPT: &str = "Summarize the conversation so far.";

/// The synthetic message that actually gets a real response after
/// compaction — the summary already carries forward whatever the model
/// needs (including the gist of whatever large content triggered
/// compaction in the first place, since the summarization call sees
/// everything up through it), so this is a plain continuation nudge, not
/// a replay of anything.
#[cfg(feature = "server")]
pub(super) const COMPACTION_CONTINUATION_PROMPT: &str = "Continue based on the summary above.";

/// The three synthetic messages one compaction pass inserts, in this
/// order — pure construction, no I/O (see `compact_conversation` for what
/// actually persists them). Three messages, not one, because Anthropic
/// requires strict user/assistant alternation *and* that `messages` start
/// with `user` — a single summary message can't satisfy both "starts with
/// user" and "ends with something real to respond to" on its own:
/// `user` (structural placeholder) -> `assistant` (the summary) -> `user`
/// (a plain continuation nudge, the thing the *next* real request
/// actually responds to). See SME-18.
#[cfg(feature = "server")]
pub(super) fn compaction_messages(
    summary: String,
    covers_through_message_id: i64,
    unanswered: &[String],
) -> [(&'static str, Vec<anthropic::ContentBlock>); 3] {
    [
        (
            "user",
            vec![anthropic::ContentBlock::CompactionPlaceholder {
                text: COMPACTION_PLACEHOLDER_PROMPT.to_string(),
            }],
        ),
        (
            "assistant",
            vec![anthropic::ContentBlock::CompactionSummary {
                summary,
                covers_through_message_id,
            }],
        ),
        (
            "user",
            vec![anthropic::ContentBlock::CompactionPlaceholder {
                text: continuation_prompt(unanswered),
            }],
        ),
    ]
}

/// The nudge after a summary. When the compaction covered messages the
/// model hasn't answered yet (the request that triggered it, notices), it
/// quotes them, so the model answers what was actually asked rather than
/// the summary's gist of it (SME-40 F3).
#[cfg(feature = "server")]
pub(super) fn continuation_prompt(unanswered: &[String]) -> String {
    if unanswered.is_empty() {
        return COMPACTION_CONTINUATION_PROMPT.to_string();
    }
    // Each quoted in part at most: a huge paste quoted whole would leave
    // the compacted request as big as the one compaction was for, and the
    // summary already covers it (SME-51 B10).
    let quoted: Vec<String> = unanswered
        .iter()
        .map(|text| match crate::fetch_guard::truncate(text.clone(), CONTINUATION_QUOTE_MAX_CHARS) {
            (kept, true) => format!("{kept}\n[cut to its first {CONTINUATION_QUOTE_MAX_CHARS} characters; the summary covers the rest]"),
            (whole, false) => whole,
        })
        .collect();
    format!(
        "{COMPACTION_CONTINUATION_PROMPT} The summary includes these latest messages, which you \
         haven't answered yet; respond to them now:\n\n{}",
        quoted.join("\n\n")
    )
}

/// How much of each unanswered message a continuation quotes.
#[cfg(feature = "server")]
pub(super) const CONTINUATION_QUOTE_MAX_CHARS: usize = 4_000;

/// The text of the user messages at the end of `messages`, after the
/// model's last reply: what a compaction happening now would summarize
/// before the model has answered it. Tool results aren't included; notices
/// and the user's own words are.
#[cfg(feature = "server")]
pub(super) fn unanswered_user_text(messages: &[Message]) -> Vec<String> {
    let mut texts: Vec<String> = messages
        .iter()
        .rev()
        .take_while(|m| m.role == "user")
        .flat_map(|m| m.blocks().unwrap_or_default())
        .filter_map(|block| match block {
            anthropic::ContentBlock::Text { text } => Some(text),
            _ => None,
        })
        .collect();
    texts.reverse();
    // What came before the user's last stop was called off (SME-51 B10).
    if let Some(stop) = texts.iter().rposition(|text| text == STOP_NOTICE) {
        texts.drain(..=stop);
    }
    texts
}

/// The system prompt for the dedicated summarization call `compact_conversation`
/// makes — no tools attached, so this is the model's only instruction.
/// Explicitly demands every live id be preserved verbatim rather than
/// trusting the model to notice one buried in a long transcript
/// unprompted — see `describe_live_state` below, which hands them over
/// directly rather than leaving them to be spotted.
#[cfg(feature = "server")]
pub(super) const COMPACTION_SYSTEM_PROMPT: &str = "You are compacting an AI coding agent's \
conversation history to free up context window space. Write a concise summary \
of the conversation so far: the user's original goal, key decisions made, the \
current state of any files or code changed, and any unresolved next steps. You \
MUST explicitly mention, by its exact id, every sandbox pod and terminal \
listed below as currently live — never omit or paraphrase an \
id, since later tool calls still need a working reference to it. Write the \
summary as plain prose.";

/// A plain-text listing of every currently-live pod/terminal for
/// `conversation_id`, handed to the summarization call so it can be told
/// directly what must survive — see `COMPACTION_SYSTEM_PROMPT` and
/// SME-18's "Interaction with live/
/// in-flight state." Best-effort: a lookup failure just omits that
/// category rather than failing the whole compaction over it.
#[cfg(feature = "server")]
pub(super) async fn describe_live_state(pool: &PgPool, conversation_id: i64) -> String {
    // Deliberately the plain `db::` row queries, not `sandbox::list_pods`/
    // `list_terminals` — those also reach the cluster to check live
    // connection status, which a summarization prompt doesn't need: just
    // which ids exist and aren't terminated.
    let mut lines = Vec::new();
    if let Ok(pods) = db::list_sandbox_pods(pool, conversation_id).await {
        for pod in pods {
            lines.push(format!("- sandbox pod_id {}", pod.id));
        }
    }
    if let Ok(terminals) = db::list_sandbox_terminals_for_conversation(pool, conversation_id).await
    {
        for terminal in terminals {
            lines.push(format!(
                "- terminal_id {} in pod_id {}",
                terminal.id, terminal.pod_id
            ));
        }
    }
    if let Ok(todos) = db::get_conversation_todos(pool, conversation_id).await {
        for todo in todos {
            lines.push(format!(
                "- todo: {} (status: {})",
                todo.content,
                todo.status.as_str()
            ));
        }
    }
    if lines.is_empty() {
        "(nothing currently live)".to_string()
    } else {
        lines.join("\n")
    }
}

/// The conversation as plain text, for the summarization call: the latest
/// compaction's summary and everything since (what came before it is
/// already in that summary — including it again made every later
/// compaction bigger than the last, until the summarization call couldn't
/// fit at all), cut to `max_chars` by dropping the oldest part.
#[cfg(feature = "server")]
pub(super) fn compaction_transcript(messages: &[Message], max_chars: usize) -> String {
    let latest_boundary = messages
        .iter()
        .filter_map(|m| m.blocks().ok())
        .flatten()
        .filter_map(|block| match block {
            anthropic::ContentBlock::CompactionSummary {
                covers_through_message_id,
                ..
            } => Some(covers_through_message_id),
            _ => None,
        })
        .max();
    let mut transcript = String::new();
    for message in messages
        .iter()
        .filter(|m| latest_boundary.is_none_or(|boundary| m.id > boundary))
    {
        let Ok(blocks) = message.blocks() else {
            continue;
        };
        for block in blocks {
            let text = match block {
                anthropic::ContentBlock::Text { text } => text,
                anthropic::ContentBlock::ToolUse { name, input, .. } => {
                    format!("[called tool {name} with {input}]")
                }
                anthropic::ContentBlock::ToolResult { content, .. } => {
                    format!("[tool result: {content}]")
                }
                anthropic::ContentBlock::Thinking { .. } => continue,
                anthropic::ContentBlock::CompactionSummary { summary, .. } => summary,
                // Purely structural (see its own doc comment) — noise for
                // a *later* compaction's own summarization transcript, not
                // real prior dialogue worth feeding back in.
                anthropic::ContentBlock::CompactionPlaceholder { .. } => continue,
            };
            transcript.push_str(&message.role);
            transcript.push_str(": ");
            transcript.push_str(&text);
            transcript.push('\n');
        }
    }
    let total = transcript.chars().count();
    if total <= max_chars {
        return transcript;
    }
    const OMITTED: &str = "[earlier conversation omitted]\n";
    let keep = max_chars.saturating_sub(OMITTED.chars().count());
    let tail: String = transcript.chars().skip(total - keep).collect();
    format!("{OMITTED}{tail}")
}

/// How much transcript the summarization call can take: the context window
/// less room for its instructions, the live-state listing and its own
/// output, at a conservative 3 characters per token.
#[cfg(feature = "server")]
pub(super) fn compaction_transcript_budget(context_window: u32) -> usize {
    (context_window as usize).saturating_sub(8_192) * 3
}

/// Runs one compaction pass: summarizes everything currently persisted (up
/// through and including whatever's pending — the size trigger fired
/// because of everything currently in the conversation, not just the
/// older part of it) via a separate, tools-less Anthropic call, and
/// inserts `compaction_messages`' three synthetic messages. Nothing
/// already stored is rewritten or removed. A no-op (`Ok(())`, no API call)
/// if there's nothing to compact, or if the current last message isn't a
/// safe boundary (shouldn't happen given the loop's own invariant — see
/// `is_safe_compaction_boundary` — but checked defensively rather than
/// assumed). If the summarization call itself fails, this propagates the
/// error and the whole turn fails loudly, rather than proceeding with the
/// oversized request compaction exists to prevent — see SME-18's
/// "Resolved" decision on this.
#[cfg(feature = "server")]
pub(super) async fn compact_conversation(
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &crate::providers::TurnModel,
) -> ServerFnResult<()> {
    let messages = db::list_messages(pool, conversation_id)
        .await
        .map_err(ServerFnError::new)?;
    let Some(last) = messages.last() else {
        return Ok(());
    };
    let last_blocks = last.blocks().map_err(ServerFnError::new)?;
    if !is_safe_compaction_boundary(&last_blocks) {
        return Ok(());
    }
    let covers_through_message_id = last.id;

    let transcript = compaction_transcript(
        &messages,
        compaction_transcript_budget(turn_model.context_window),
    );

    let live_state = describe_live_state(pool, conversation_id).await;
    let prompt = format!(
        "Conversation so far:\n{transcript}\n\nCurrently live (preserve these \
         exact ids in your summary):\n{live_state}"
    );

    let summarization_request = anthropic::CreateMessageRequest {
        model: turn_model.model.clone(),
        max_tokens: 2048,
        system: Some(COMPACTION_SYSTEM_PROMPT.to_string()),
        messages: vec![anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text { text: prompt }],
        }],
        stream: true,
        tools: vec![],
        thinking: None,
    };

    let mut discard_deltas = |_: &str| {};
    let turn = anthropic::stream::stream_anthropic_message(
        &turn_model.endpoint,
        &summarization_request,
        &mut discard_deltas,
    )
    .await
    .map_err(ServerFnError::new)?;
    let summary = turn
        .content
        .into_iter()
        .find_map(|block| match block {
            anthropic::ContentBlock::Text { text } => Some(text),
            _ => None,
        })
        .unwrap_or_default();

    let unanswered = unanswered_user_text(&messages);
    for (role, content) in compaction_messages(summary, covers_through_message_id, &unanswered) {
        let saved = db::create_message(pool, conversation_id, role, &content)
            .await
            .map_err(ServerFnError::new)?;
        // Watching tabs show the divider as it happens (SME-40 F5).
        record_saved(conversation_id, &mut Vec::new(), saved);
    }
    // The usage that triggered this described the old, long history. Kept,
    // a failed or stopped next call would have the following turn compact
    // again at once; the next real reply records the new size (SME-51 B10).
    db::clear_conversation_usage(pool, conversation_id)
        .await
        .map_err(ServerFnError::new)?;
    Ok(())
}
