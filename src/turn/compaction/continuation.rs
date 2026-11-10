//! What the compaction leaves the model to carry on with: the three
//! synthetic messages, the continuation prompt and its typed-word quotes,
//! and the unanswered tail. Part of `turn::compaction` (split out of its
//! `compaction.rs`, SME-443); names unchanged, visibilities re-exported
//! from `mod.rs` at the same reach.

use super::*;

/// The three synthetic messages one compaction pass inserts, in this
/// order — pure construction, no I/O (see `compact_conversation` for what
/// actually persists them). Three messages, not one, because Anthropic
/// requires strict user/assistant alternation *and* that `messages` start
/// with `user` — a single summary message can't satisfy both "starts with
/// user" and "ends with something real to respond to" on its own:
/// `user` (structural placeholder) -> `assistant` (the summary) -> `user`
/// (a plain continuation nudge, the thing the *next* real request
/// actually responds to). See SME-18.
///
/// `refused`, when set, is why this compaction happened instead of the
/// proactive one — the overflow refusal's reason, stored on the summary
/// block for the transcript divider to read (SME-73).
#[cfg(feature = "server")]
pub fn compaction_messages(
    summary: String,
    covers_through_message_id: i64,
    typed: &Typed,
    unanswered: &[String],
    todos: &[anthropic::tools::TodoItem],
    refused: Option<String>,
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
                refused,
            }],
        ),
        (
            "user",
            vec![anthropic::ContentBlock::CompactionPlaceholder {
                text: continuation_prompt(typed, unanswered, todos),
            }],
        ),
    ]
}

/// The nudge after a summary. When the compaction covered messages the
/// model hasn't answered yet (the request that triggered it, notices), it
/// quotes them, so the model answers what was actually asked rather than
/// the summary's gist of it (SME-40 F3).
/// It also quotes, word for word, what the user (or the agent's parent)
/// wrote earlier (`typed`, SME-367): a summary can paraphrase a mid-run
/// correction away, and on SME-73 a seat whose correction it lost made the
/// same mistake again after two compactions. And it pins the conversation's
/// todo list verbatim (`todos`, SME-462) — the model's own progress tracker,
/// which a summary can drop and `todoread` only restores if the model
/// thinks to ask.
#[cfg(feature = "server")]
pub fn continuation_prompt(
    typed: &Typed,
    unanswered: &[String],
    todos: &[anthropic::tools::TodoItem],
) -> String {
    let mut prompt = COMPACTION_CONTINUATION_PROMPT.to_string();
    if !typed.quotes.is_empty() {
        prompt.push_str(
            " For reference, what the user (or your parent) wrote earlier in this conversation, \
             word for word, with their answers to your questions: the corrections and constraints \
             in them still hold, but the summary marks the requests it reports done, so don't \
             redo those without checking them first.\n\n",
        );
        prompt.push_str(&typed.quotes.join(QUOTE_SEPARATOR));
        if typed.omitted > 0 {
            prompt.push_str(&format!(
                "\n\n({} more they wrote in between {} left out here; the summary covers {}.)",
                typed.omitted,
                if typed.omitted == 1 { "is" } else { "are" },
                if typed.omitted == 1 { "it" } else { "them" },
            ));
        }
    }
    // The pinned list sits beside the typed quotes — persisted transcript
    // content, riding the moving cache breakpoint exactly as they do — and
    // before the unanswered tail, which stays last with its "respond to
    // them now" (SME-462).
    let pinned = pinned_todos_text(todos);
    if let Some(block) = &pinned {
        prompt.push_str("\n\n");
        prompt.push_str(block);
    }
    if unanswered.is_empty() {
        return prompt;
    }
    let quoted: Vec<String> = unanswered.iter().map(|text| quote(text)).collect();
    format!(
        "{prompt}{}The summary includes these latest messages, which you haven't answered yet; \
         respond to them now:\n\n{}",
        if typed.quotes.is_empty() && pinned.is_none() { " " } else { "\n\n" },
        quoted.join("\n\n")
    )
}

/// One message as a continuation quotes it: in part at most, since a huge
/// paste quoted whole would leave the compacted request as big as the one
/// compaction was for, and the summary already covers it (SME-51 B10).
#[cfg(feature = "server")]
fn quote(text: &str) -> String {
    quote_to(text, CONTINUATION_QUOTE_MAX_CHARS, SUMMARY_COVERS_THE_REST)
}

/// What a continuation's cut quote says of the rest. Not for the
/// summariser's own input, where there's no summary yet.
#[cfg(feature = "server")]
pub const SUMMARY_COVERS_THE_REST: &str = "; the summary covers the rest";

/// `text` cut to at most `max` characters, the cut marked, with `rest` saying
/// where the rest went.
#[cfg(feature = "server")]
pub fn quote_to(text: &str, max: usize, rest: &str) -> String {
    match crate::fetch_guard::truncate(text.to_string(), max) {
        (kept, true) => format!("{kept}\n[cut to its first {max} characters{rest}]"),
        (whole, false) => whole,
    }
}

/// Between two quoted typed messages; counted in the budget.
#[cfg(feature = "server")]
pub const QUOTE_SEPARATOR: &str = "\n\n---\n\n";

/// What a continuation quotes of the user's and the parent's own words
/// (SME-367): `quotes`, oldest first and already cut to size, and how many
/// it left out.
#[cfg(feature = "server")]
#[derive(Debug, Default, PartialEq)]
pub struct Typed {
    pub quotes: Vec<String>,
    pub omitted: usize,
}

/// How much of the user's and the parent's earlier words a continuation
/// quotes in all, separators included, each also held to
/// `CONTINUATION_QUOTE_MAX_CHARS`: about 3,000 tokens, so a long run's
/// quotes can't fill the window. A smaller window gets an eighth of its
/// size in characters, about a thirtieth of it in tokens (reviews 1 and 2:
/// a fixed 12,000 left a 28k-token window over its trigger after every
/// compaction, and a quarter left it at it).
#[cfg(feature = "server")]
pub const CONTINUATION_TYPED_MAX_CHARS: usize = 12_000;

/// The typed-words budget for a model with `context_window` tokens.
#[cfg(feature = "server")]
pub fn typed_budget(context_window: u32) -> usize {
    CONTINUATION_TYPED_MAX_CHARS.min(usize::try_from(context_window).unwrap_or(usize::MAX) / 8)
}

/// How a parent's `message_agent` reaches its child (`turn::notify`'s
/// `agent_message_text`, the label `agents::messages` gives a parent).
#[cfg(feature = "server")]
const PARENT_MESSAGE_START: &str = "Message from your parent:";

/// The starts of the user-role texts smelt writes for other agents: what a
/// child reports, messages, or asks (`Report from `, `Message from agent #`,
/// `Question from `). They're another agent's typed words, shown as bubbles
/// as written, so they carry no `NOTICE_MARK` — marking them would grey
/// real agent messages in the transcript (SME-357's trade-off). Everything
/// smelt writes itself now carries the mark and is excluded by it, not by
/// words — one exception: the deciding `"The user trusts …"` wording in
/// `git::server::trust` stays unmarked on purpose, quoted as the user's own
/// news. A text this list misses is quoted as if typed, which costs space,
/// never a correction.
#[cfg(feature = "server")]
const SERVER_TEXT_STARTS: &[&str] = &["Report from ", "Message from agent #", "Question from "];

/// Whether a user-role text is the user's own words or a parent's message:
/// not a marked notice (every notice smelt saves into a transcript carries
/// `NOTICE_MARK` since SME-357, its deciding unmarked `"The user trusts …"`
/// wording excepted), and not one of `SERVER_TEXT_STARTS`.
#[cfg(feature = "server")]
pub fn is_typed(text: &str) -> bool {
    if text.starts_with(PARENT_MESSAGE_START) {
        return true;
    }
    !text.starts_with(crate::api::chat::NOTICE_MARK) && !SERVER_TEXT_STARTS.iter().any(|start| text.starts_with(start))
}

/// The user's and the parent's own words in `messages`, all of the
/// conversation's (an earlier compaction's summary may have paraphrased
/// them), up to its unanswered tail, which the continuation quotes as such
/// (SME-367): their typed messages, and their answers to the model's
/// `ask_user` and `ask_parent` calls (review 1: those are often the
/// decisions; a refused call's error result isn't one). A request the user
/// stopped isn't quoted: a stop, in the tail too, drops what the user
/// typed since the model's last final reply (review 2). The first, the original ask, is always
/// kept, to half the budget; the rest newest first, each that still fits
/// `typed_budget(context_window)`, then put back in order.
#[cfg(feature = "server")]
pub fn typed_user_text(messages: &[Message], context_window: u32) -> Typed {
    typed_within(messages, typed_budget(context_window), SUMMARY_COVERS_THE_REST)
}

/// `typed_user_text` in `budget` characters, a cut quote saying `after_cut`.
pub fn typed_within(messages: &[Message], budget: usize, after_cut: &str) -> Typed {
    let answered = messages.len() - messages.iter().rev().take_while(|m| m.role == "user").count();
    let mut questions = std::collections::HashSet::new();
    let mut all: Vec<String> = Vec::new();
    // How many of `all` a final reply (one with no tool call) has answered:
    // a stop calls off only what came after it (review 2: a run the model
    // had answered, a correction among it, isn't the stopped request). A
    // stop straight after a reply with no call is that reply stopped (its
    // part saved), so it calls off what that reply was answering:
    // `answered_before` is the count before it.
    let (mut answered_upto, mut answered_before, mut last_was_final) = (0, 0, false);
    for (index, message) in messages.iter().enumerate() {
        let blocks = message.blocks().unwrap_or_default();
        if message.role != "user" {
            let mut calls = false;
            for block in blocks {
                if let anthropic::ContentBlock::ToolUse { id, name, .. } = block {
                    calls = true;
                    if name == crate::questions::ASK_USER || name == crate::agents::ASK_PARENT {
                        questions.insert(id);
                    }
                }
            }
            if calls {
                last_was_final = false;
            } else {
                (answered_before, answered_upto, last_was_final) = (answered_upto, all.len(), true);
            }
            continue;
        }
        // The unanswered tail is quoted as such; only its stops count here
        // (review 2: a stop there still calls the earlier request off).
        // The stop's note is compared through `users_text` so both saved
        // forms count: the marked one written since SME-357 and the legacy
        // unmarked row.
        let in_tail = index >= answered;
        let stops_here = blocks.iter().any(|b| {
            matches!(b, anthropic::ContentBlock::Text { text }
                if crate::api::chat::users_text(text) == STOP_NOTICE)
        });
        if !stops_here {
            last_was_final = false;
        }
        for block in blocks {
            let text = match block {
                anthropic::ContentBlock::Text { text } if crate::api::chat::users_text(&text) == STOP_NOTICE => {
                    let upto = if last_was_final { answered_before } else { answered_upto };
                    all.truncate(upto);
                    answered_upto = all.len();
                    continue;
                }
                _ if in_tail => continue,
                anthropic::ContentBlock::Text { text } if is_typed(&text) && !text.trim().is_empty() => text,
                anthropic::ContentBlock::ToolResult { tool_use_id, content, is_error }
                    if questions.contains(&tool_use_id)
                        && is_error != Some(true)
                        && content != crate::questions::NOT_ANSWERED
                        && content != crate::questions::NOT_ANSWERED_BY_PARENT =>
                {
                    content
                }
                _ => continue,
            };
            all.push(text);
        }
    }
    let Some((first, rest)) = all.split_first() else {
        return Typed::default();
    };
    let first = quote_to(first, (budget / 2).max(1), after_cut);
    let mut used = first.chars().count();
    let mut newest: Vec<String> = Vec::new();
    for text in rest.iter().rev() {
        let quoted = quote_to(text, CONTINUATION_QUOTE_MAX_CHARS.min(budget), after_cut);
        let size = quoted.chars().count() + QUOTE_SEPARATOR.len();
        // One that doesn't fit is left out alone: older, shorter ones may
        // still fit (SME-386 T8).
        if used + size > budget {
            continue;
        }
        used += size;
        newest.push(quoted);
    }
    let omitted = rest.len() - newest.len();
    newest.reverse();
    let mut quotes = vec![first];
    quotes.extend(newest);
    Typed { quotes, omitted }
}

/// How much of each unanswered message a continuation quotes.
#[cfg(feature = "server")]
pub const CONTINUATION_QUOTE_MAX_CHARS: usize = 4_000;

/// The text of the user messages at the end of `messages`, after the
/// model's last reply: what a compaction happening now would summarize
/// before the model has answered it. Tool results aren't included; notices
/// and the user's own words are.
#[cfg(feature = "server")]
pub fn unanswered_user_text(messages: &[Message]) -> Vec<String> {
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
    // Both saved stop notes count, marked (since SME-357) and legacy.
    if let Some(stop) = texts.iter().rposition(|text| crate::api::chat::users_text(text) == STOP_NOTICE) {
        texts.drain(..=stop);
    }
    texts
}

/// `texts` (`unanswered_user_text`) held to `budget` characters in all, so a
/// compaction with many messages waiting doesn't leave its next request
/// over the trigger for that alone (SME-386 review L1). The newest are kept:
/// it's what the model answers. What's left out is said at the start.
#[cfg(feature = "server")]
pub fn cap_unanswered(texts: Vec<String>, budget: usize) -> Vec<String> {
    let mut left = budget;
    let mut kept = Vec::new();
    let total = texts.len();
    for text in texts.into_iter().rev() {
        let size = text.chars().count().min(CONTINUATION_QUOTE_MAX_CHARS);
        if size <= left {
            left -= size;
            kept.push(text);
        } else if kept.is_empty() {
            // The one to answer gets what there is.
            left = 0;
            kept.push(quote_to(&text, budget, SUMMARY_COVERS_THE_REST));
        } else {
            break;
        }
    }
    if kept.len() < total {
        let omitted = total - kept.len();
        kept.push(format!("({omitted} earlier {} left out here; the summary covers {}.)",
            if omitted == 1 { "message is" } else { "messages are" },
            if omitted == 1 { "it" } else { "them" },
        ));
    }
    kept.reverse();
    kept
}

/// The cap on the pinned todo block (SME-462): about 500 tokens. A constant,
/// not window-scaled like `typed_budget` — an eighth of a 16k-token window
/// already exceeds it, so the small-window rule (SME-386) never binds below
/// it.
#[cfg(feature = "server")]
pub const CONTINUATION_TODOS_MAX_CHARS: usize = 2_000;

/// The pinned todo block's lead-in (SME-462): what the list is — the model's
/// own tracker, echoed back to it — and what it isn't: verified state. It
/// echoes, and cannot weaken, SME-393's verify-before-act nudge. Tests pin
/// the marker phrase `not verified facts`, not this whole sentence.
#[cfg(feature = "server")]
const PINNED_TODOS_LEAD_IN: &str = "Your own todo list as of this compaction — your own tracker, \
    i.e. your own claims about your progress, not verified facts: a `completed` here is not \
    evidence the work was done (verify as you verify the summary), and it is a snapshot of \
    this moment, not the live panel.";

/// What a cut todo says of the rest of its line (SME-462) — the same pointer
/// as the trailing note gives.
#[cfg(feature = "server")]
const TODOS_REST_POINTER: &str = "; todoread lists every todo whole";

/// The conversation's todo list as a block pinned into a continuation
/// message (SME-462): each item verbatim in list order as
/// `- {content} ({status})`, under the lead-in marking them as the model's
/// own claims. `pub(crate)` rather than turn's usual `pub(super)` reach
/// because the context view calls this same builder, so the view can't
/// drift from what a compaction would pin. `None` for an empty list — a conversation that never used
/// todos gains no section. Held to `CONTINUATION_TODOS_MAX_CHARS`: whole
/// items in order while they fit, the first that doesn't fit kept in part
/// via `quote_to` at what's left of the budget, and what's dropped said at
/// the end in `cap_unanswered`'s grammar — a cut always names the `todoread`
/// pointer to the full DB list.
#[cfg(feature = "server")]
pub(crate) fn pinned_todos_text(todos: &[anthropic::tools::TodoItem]) -> Option<String> {
    if todos.is_empty() {
        return None;
    }
    let lines: Vec<String> = todos
        .iter()
        .map(|todo| format!("- {} ({})", todo.content, todo.status.as_str()))
        .collect();
    let whole = lines.join("\n");
    if whole.chars().count() <= CONTINUATION_TODOS_MAX_CHARS {
        return Some(format!("{PINNED_TODOS_LEAD_IN}\n\n{whole}"));
    }
    let mut kept: Vec<String> = Vec::new();
    let mut used = 0usize;
    for line in &lines {
        let size = line.chars().count();
        let separator = usize::from(!kept.is_empty());
        if used + separator + size <= CONTINUATION_TODOS_MAX_CHARS {
            used += separator + size;
            kept.push(line.clone());
            continue;
        }
        // The first item that doesn't fit whole is kept in part: its head
        // still says what the todo is, to what the budget has left.
        let remaining = CONTINUATION_TODOS_MAX_CHARS - (used + separator);
        if remaining > 0 {
            kept.push(quote_to(line, remaining, TODOS_REST_POINTER));
        }
        break;
    }
    if kept.len() < lines.len() {
        let dropped = lines.len() - kept.len();
        kept.push(format!(
            "({} of {} todos {} shown here; the rest {} left out here; todoread lists the whole list.)",
            kept.len(),
            lines.len(),
            if kept.len() == 1 { "is" } else { "are" },
            if dropped == 1 { "is" } else { "are" },
        ));
    }
    Some(format!("{PINNED_TODOS_LEAD_IN}\n\n{}", kept.join("\n")))
}
