//! Compacting a conversation that nears its model's context window.

use super::*;

/// Headroom for the token estimate's error: kept off the context window
/// on top of the reply (`reply_budget`), and the compaction trigger's
/// floor should `MIN_REPLY_TOKENS` ever be smaller — mirrors opencode's
/// own `max(output reserve, buffer)` term. See SME-18.
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

/// The size of request that triggers a compaction: the context window less
/// what's kept for the reply (model output allowance + a safety buffer) —
/// opencode's own trigger mechanism, checked *before* a request is sent
/// rather than reacting to a real rejection. See SME-18. Once compacting
/// can't help, `CompactionProgress` moves it nearer the window.
#[cfg(feature = "server")]
pub(super) fn compaction_ceiling(context_window: u32) -> u64 {
    let reserved = MIN_REPLY_TOKENS.max(COMPACTION_SAFETY_BUFFER) as u64;
    (context_window as u64).saturating_sub(reserved)
}

/// The next request's input as the last real usage measured it, plus an
/// estimate of what's new since: everything the last call read and wrote.
/// `None` when nothing has been measured since the history last changed
/// shape (a first request, or one just after a compaction).
#[cfg(feature = "server")]
pub(super) fn projected_input(last_usage: Option<&anthropic::TokenUsage>, new_content_estimate: u64) -> Option<u64> {
    let last_usage = last_usage?;
    let already_used = (last_usage.input_tokens
        + last_usage.output_tokens
        + last_usage.cache_creation_input_tokens
        + last_usage.cache_read_input_tokens)
        .max(0) as u64;
    Some(already_used + new_content_estimate)
}

/// An estimate of a whole request's input: its messages, system prompt
/// and tool definitions, at `estimate_tokens`' 4 characters a token. For a
/// request no usage measured (`projected_input`).
#[cfg(feature = "server")]
pub(super) fn estimate_request_tokens(request: &anthropic::CreateMessageRequest) -> u64 {
    let messages: u64 = request.messages.iter().map(|m| estimate_tokens(&m.content)).sum();
    let system = request.system.as_ref().map_or(0, |s| s.len() / 4) as u64;
    let tools = serde_json::to_string(&request.tools).map_or(0, |t| t.len() / 4) as u64;
    messages + system + tools
}

/// `budget` held to the room a request of `projected_input` tokens really
/// leaves, for one past the trigger because compacting can't help: the
/// floor in `reply_budget` counts on the trigger having kept that room, and
/// a reply it doesn't have is a refusal (SME-386 M3).
#[cfg(feature = "server")]
pub(super) fn budget_within_room(budget: ReplyBudget, context_window: u32, projected_input: u64) -> ReplyBudget {
    let room = (context_window as u64)
        .saturating_sub(projected_input)
        .saturating_sub(COMPACTION_SAFETY_BUFFER as u64);
    let room = u32::try_from(room).unwrap_or(u32::MAX).max(1);
    if room < budget.tokens {
        ReplyBudget { tokens: room, limit: crate::api::chat::ReplyLimit::RoomLeft }
    } else {
        budget
    }
}

/// How many tokens a reply may use (SME-111): the smallest of the model's
/// output cap, where known; the room the request leaves in the window,
/// less `COMPACTION_SAFETY_BUFFER`; and half the window, so a single reply
/// can't crowd the rest of the history out of the next compaction. Never
/// below `MIN_REPLY_TOKENS` (or half a smaller window), as every turn
/// asked for before; the compaction trigger keeps that much room. A request
/// let past the trigger gets `budget_within_room` on top, which can be less.
#[cfg(feature = "server")]
pub(super) fn reply_budget(context_window: u32, projected_input: u64, output_cap: Option<u32>) -> ReplyBudget {
    let half = context_window / 2;
    let room = (context_window as u64)
        .saturating_sub(projected_input)
        .saturating_sub(COMPACTION_SAFETY_BUFFER as u64);
    let room = u32::try_from(room).unwrap_or(u32::MAX);
    let budget = room.min(half).max(MIN_REPLY_TOKENS.min(half));
    let tokens = output_cap.map_or(budget, |cap| budget.min(cap)).max(1);
    let limit = if output_cap == Some(tokens) {
        crate::api::chat::ReplyLimit::OutputCap
    } else if tokens == half {
        crate::api::chat::ReplyLimit::HalfWindow
    } else {
        // The room left, or the floor kept when there's even less.
        crate::api::chat::ReplyLimit::RoomLeft
    };
    ReplyBudget { tokens, limit }
}

/// A reply's budget, and what bound it (for a cut-off notice).
#[cfg(feature = "server")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ReplyBudget {
    pub(super) tokens: u32,
    pub(super) limit: crate::api::chat::ReplyLimit,
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
///
/// `refused`, when set, is why this compaction happened instead of the
/// proactive one — the overflow refusal's reason, stored on the summary
/// block for the transcript divider to read (SME-73).
#[cfg(feature = "server")]
pub(super) fn compaction_messages(
    summary: String,
    covers_through_message_id: i64,
    typed: &Typed,
    unanswered: &[String],
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
                text: continuation_prompt(typed, unanswered),
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
/// same mistake again after two compactions.
#[cfg(feature = "server")]
pub(super) fn continuation_prompt(typed: &Typed, unanswered: &[String]) -> String {
    let mut prompt = COMPACTION_CONTINUATION_PROMPT.to_string();
    if !typed.quotes.is_empty() {
        prompt.push_str(
            " For reference, what the user (or your parent) wrote earlier in this conversation, \
             word for word, with their answers to your questions: the corrections and constraints \
             in them still hold, but the summary says which requests are already done, so don't \
             redo those.\n\n",
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
    if unanswered.is_empty() {
        return prompt;
    }
    let quoted: Vec<String> = unanswered.iter().map(|text| quote(text)).collect();
    format!(
        "{prompt}{}The summary includes these latest messages, which you haven't answered yet; \
         respond to them now:\n\n{}",
        if typed.quotes.is_empty() { " " } else { "\n\n" },
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
pub(super) const SUMMARY_COVERS_THE_REST: &str = "; the summary covers the rest";

/// `text` cut to at most `max` characters, the cut marked, with `rest` saying
/// where the rest went.
#[cfg(feature = "server")]
pub(super) fn quote_to(text: &str, max: usize, rest: &str) -> String {
    match crate::fetch_guard::truncate(text.to_string(), max) {
        (kept, true) => format!("{kept}\n[cut to its first {max} characters{rest}]"),
        (whole, false) => whole,
    }
}

/// Between two quoted typed messages; counted in the budget.
#[cfg(feature = "server")]
pub(super) const QUOTE_SEPARATOR: &str = "\n\n---\n\n";

/// What a continuation quotes of the user's and the parent's own words
/// (SME-367): `quotes`, oldest first and already cut to size, and how many
/// it left out.
#[cfg(feature = "server")]
#[derive(Debug, Default, PartialEq)]
pub(super) struct Typed {
    pub(super) quotes: Vec<String>,
    pub(super) omitted: usize,
}

/// How much of the user's and the parent's earlier words a continuation
/// quotes in all, separators included, each also held to
/// `CONTINUATION_QUOTE_MAX_CHARS`: about 3,000 tokens, so a long run's
/// quotes can't fill the window. A smaller window gets an eighth of its
/// size in characters, about a thirtieth of it in tokens (reviews 1 and 2:
/// a fixed 12,000 left a 28k-token window over its trigger after every
/// compaction, and a quarter left it at it).
#[cfg(feature = "server")]
pub(super) const CONTINUATION_TYPED_MAX_CHARS: usize = 12_000;

/// The typed-words budget for a model with `context_window` tokens.
#[cfg(feature = "server")]
pub(super) fn typed_budget(context_window: u32) -> usize {
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
pub(super) fn is_typed(text: &str) -> bool {
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
pub(super) fn typed_user_text(messages: &[Message], context_window: u32) -> Typed {
    typed_within(messages, typed_budget(context_window), SUMMARY_COVERS_THE_REST)
}

/// `typed_user_text` in `budget` characters, a cut quote saying `after_cut`.
pub(super) fn typed_within(messages: &[Message], budget: usize, after_cut: &str) -> Typed {
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
pub(super) fn cap_unanswered(texts: Vec<String>, budget: usize) -> Vec<String> {
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

/// The most a summary may write.
#[cfg(feature = "server")]
pub(super) const SUMMARY_MAX_TOKENS: u32 = 2048;

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
///
/// `refused`, when set, is why the turn needed this compaction rather
/// than waiting for the proactive check — an overflow refusal's reason
/// (from `overflow::refused_reason`), saved on the summary block so the
/// transcript divider reads it (SME-73). A compaction whose *own* summary
/// request was refused and recovered fills in its own reason when no one
/// else named one.
#[cfg(feature = "server")]
pub(super) async fn compact_conversation(
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &mut crate::providers::TurnModel,
    slot: anthropic::stream::SlotPolicy<'_>,
    refused: Option<String>,
) -> ServerFnResult<()> {
    use tracing::field::Empty;
    // Its model call is its child (SME-137).
    let span = tracing::info_span!(
        "compaction",
        conversation_id,
        otel.status_code = Empty,
        otel.status_description = Empty,
    );
    crate::telemetry::in_span(span.clone(), async move {
        let result = compact(pool, conversation_id, turn_model, slot, refused).await;
        if let Err(e) = &result {
            crate::telemetry::mark_error(&span, &e.to_string());
        }
        result
    })
    .await
}

/// `compact_conversation`'s work, in its span.
async fn compact(
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &mut crate::providers::TurnModel,
    slot: anthropic::stream::SlotPolicy<'_>,
    refused: Option<String>,
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

    // A summary request refused as an overflow (SME-73) is that same
    // refusal, and gets the same one answer here: learn the window it
    // names and ask once more with the transcript sized to it — half
    // the refused one when no window was named. A second refusal ends
    // the turn with the reason.
    let mut refused = refused;
    let summary = match summarize(pool, conversation_id, turn_model, slot, &transcript).await {
        Ok(summary) => summary,
        Err(anthropic::stream::ModelError::Overflow(overflow)) => {
            let named = super::overflow::learn(pool, conversation_id, turn_model, overflow.window).await;
            let budget = named.map_or(transcript.chars().count() / 2, compaction_transcript_budget);
            let transcript = compaction_transcript(&messages, budget);
            match summarize(pool, conversation_id, turn_model, slot, &transcript).await {
                Ok(summary) => {
                    // The refusal this compaction answered, when its
                    // caller didn't name one: the divider reads the
                    // window this refusal learned (SME-73).
                    refused = refused.or_else(|| Some(super::overflow::refused_reason(named)));
                    summary
                }
                Err(anthropic::stream::ModelError::Overflow(again)) => {
                    super::overflow::learn(pool, conversation_id, turn_model, again.window).await;
                    let window = again
                        .window
                        .or(named)
                        .map(|w| format!(" ({} tokens)", super::overflow::grouped(u64::from(w))))
                        .unwrap_or_default();
                    return Err(ServerFnError::new(format!(
                        "Couldn't compact the conversation: even the summary request didn't fit \
                         `{model}`'s context window{window}. The server said: {said}",
                        model = turn_model.model,
                        said = again.message
                    )));
                }
                Err(e) => return Err(ServerFnError::new(e)),
            }
        }
        Err(e) => return Err(ServerFnError::new(e)),
    };

    let unanswered = cap_unanswered(unanswered_user_text(&messages), typed_budget(turn_model.context_window));
    let typed = typed_user_text(&messages, turn_model.context_window);
    for (role, content) in compaction_messages(summary, covers_through_message_id, &typed, &unanswered, refused) {
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
    crate::claude_cli::session::reset(pool, conversation_id, turn_model.kind).await;
    Ok(())
}

/// The summarization call itself: a plain, tools-less request over the
/// transcript, in its own recorder (a refused attempt is recorded by
/// its drop, SME-178). Answers with the summary's text, or with the
/// typed error so the caller can answer an overflow (SME-73).
async fn summarize(
    pool: &PgPool,
    conversation_id: i64,
    turn_model: &crate::providers::TurnModel,
    slot: anthropic::stream::SlotPolicy<'_>,
    transcript: &str,
) -> Result<String, anthropic::stream::ModelError> {
    let live_state = describe_live_state(pool, conversation_id).await;
    let prompt = format!(
        "Conversation so far:\n{transcript}\n\nCurrently live (preserve these \
         exact ids in your summary):\n{live_state}"
    );

    let summarization_request = anthropic::CreateMessageRequest {
        model: turn_model.model.clone(),
        max_tokens: SUMMARY_MAX_TOKENS,
        system: Some(COMPACTION_SYSTEM_PROMPT.to_string()),
        messages: vec![anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text { text: prompt }],
        }],
        stream: true,
        tools: vec![],
        thinking: None,
        prompt_caching: false,
        output_config: None,
        // Thinking off, said so a llama.cpp template doesn't reason
        // anyway and spend the summary's tokens on it (SME-111).
        chat_template_kwargs: turn_model.chat_template_kwargs(false),
    };

    let mut discard_deltas = |_: &str| {};
    let recorder =
        CallRecorder::new(pool, conversation_id, turn_model, db::ModelCallKind::Compaction, &summarization_request);
    let turn = super::call::stream_for_kind(
        conversation_id, turn_model,
        &summarization_request,
        &mut discard_deltas,
        recorder.meter(),
        slot,
    )
    .await?;
    // Recorded as soon as it completes: the tokens are spent whether or
    // not the summary is saved below (SME-106). One stopped or failed
    // before then is recorded by the recorder's drop (SME-178).
    recorder.finish(&turn, None).await.map_err(|e| anthropic::stream::ModelError::Other(e.to_string()))?;
    let summary = turn
        .content
        .into_iter()
        .find_map(|block| match block {
            anthropic::ContentBlock::Text { text } => Some(text),
            _ => None,
        })
        .unwrap_or_default();
    // Never saved over the history: a summary that's empty (its thinking
    // spent the cap) or cut off fails the compaction (PR #291 review 1 M1).
    let why = if summary.trim().is_empty() { Some("came back empty") } else { anthropic::stream::is_cut_off(&turn.stop_reason).then_some("was cut off") };
    if let Some(why) = why {
        return Err(anthropic::stream::ModelError::Other(format!("Couldn't compact the conversation: its summary {why}, so the history was kept as it is. Send another message to try again.")));
    }
    Ok(summary)
}
