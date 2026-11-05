//! The conversation as the summarization call reads it (`compact`), cut to
//! what that call can take.

use super::*;

/// How the transcript marks what it cut.
const OMITTED: &str = "[earlier conversation omitted]\n";

/// What heads the user's words in a transcript. A summariser has to tell an
/// ask from its being done (SME-393), so it's told these are the asks.
const USERS_WORDS_HEADING: &str = "What the user (or this agent's parent) wrote, word for word, whatever is cut below. \
    These are requests, not a record of what was done, which the rest says; keep their goal, corrections and limits \
    in the summary:\n\n";

/// The conversation as plain text, for the summarization call: the user's
/// own words, the latest compaction's summary and everything since (what
/// came before it is already in that summary — including it again made
/// every later compaction bigger than the last, until the summarization
/// call couldn't fit at all), `max_chars` in all.
///
/// A long turn's tail is what's cut, oldest first. It used to take the
/// user's ask and the last summary with it, the two things the next summary
/// most needs: on SME-386 F11 a summary said "the original request was
/// omitted", and on F10 "then stop" fell out of one summary after another.
/// So those two come first, a quarter of the budget each at most, and the
/// tail gets what's left. A first compaction that fits whole has it all
/// word for word already, and is sent as it is.
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
    let mut summary = String::new();
    let mut since = String::new();
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
                anthropic::ContentBlock::CompactionSummary { summary: text, .. } => {
                    summary = format!("{}: {text}\n", message.role);
                    continue;
                }
                // Purely structural (see its own doc comment) — noise for
                // a *later* compaction's own summarization transcript, not
                // real prior dialogue worth feeding back in.
                anthropic::ContentBlock::CompactionPlaceholder { .. } => continue,
            };
            since.push_str(&message.role);
            since.push_str(": ");
            since.push_str(&text);
            since.push('\n');
        }
    }
    let since_chars = since.chars().count();
    if latest_boundary.is_none() && since_chars <= max_chars {
        return since;
    }
    let mut head = users_words(messages, max_chars / 4);
    head.push_str(&first_chars(&summary, max_chars / 4));
    let room = max_chars.saturating_sub(head.chars().count());
    if since_chars <= room {
        head.push_str(&since);
    } else {
        // Held to `max_chars` even when the budget has no room for the mark.
        head.push_str(OMITTED);
        let keep = room.saturating_sub(OMITTED.chars().count());
        head.extend(since.chars().skip(since_chars - keep));
    }
    first_chars(&head, max_chars)
}

/// `text`, or its first `max` characters.
fn first_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// The user's and the parent's words for the summary to keep, in about
/// `budget` characters: what they wrote that hasn't been answered yet (a
/// huge paste's start is its ask, and the tail keeps only its end), up to
/// half of it, and what `typed_within` quotes of the rest. Empty if they
/// wrote nothing still standing.
fn users_words(messages: &[Message], budget: usize) -> String {
    let mut latest = Vec::new();
    let mut used = 0;
    for text in unanswered_user_text(messages).iter().filter(|text| is_typed(text)) {
        let quoted = quote_to(text, (budget / 4).max(1));
        used += quoted.chars().count() + QUOTE_SEPARATOR.len();
        if used > budget / 2 {
            break;
        }
        latest.push(quoted);
    }
    let typed = typed_within(messages, budget.saturating_sub(used));
    let omitted = typed.omitted;
    let mut quotes = typed.quotes;
    quotes.extend(latest);
    if quotes.is_empty() {
        return String::new();
    }
    let left_out = if omitted > 0 { format!("\n\n({omitted} more left out here.)") } else { String::new() };
    format!("{USERS_WORDS_HEADING}{}{left_out}\n\n", quotes.join(QUOTE_SEPARATOR))
}

/// How much transcript the summarization call can take: the context window
/// less room for its instructions, the live-state listing and its own
/// output, at a conservative 3 characters per token.
pub(super) fn compaction_transcript_budget(context_window: u32) -> usize {
    (context_window as usize).saturating_sub(8_192) * 3
}
